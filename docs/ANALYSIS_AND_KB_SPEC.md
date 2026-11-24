# Analysis Engine, IR, Proof Obligations, and Knowledge Bases

> **What this document is**
> The technical supplement to [ARCHITECTURE.md](../ARCHITECTURE.md): the frontend and IR, the
> Advisory lane's summaries and rules, the Soundness lane's abstract domain and proof
> obligations, the knowledge bases, the Ratchet baseline, profile dumps, and the Stage 0-A
> measurements. Crates are named `heki_*` (`crates/heki_ast` to `crates/heki_lsp`), and
> configuration keys live under `[tool.heki]` in kebab-case. Parts marked **(planned)** are
> designed but not implemented.

---

## 1. Frontend, semantic analysis, and the SSA-CFG IR (`heki_ast` / `heki_semantic` / `heki_ir`)

### 1.1 Scopes and syntax by Python version (`heki_semantic`)
`ruff_python_parser` / `ruff_python_ast` are isolated in `heki_ast`; scopes, bindings, and module
resolution are heki's own (`heki_semantic`).

- **Version-dependent scope rules (driven by `RuntimeProfile`)**:
  - **3.12+ PEP 709**: `list` / `set` / `dict` comprehensions are inlined into the enclosing
    function or module scope (with the exact exceptions, such as directly in a class body).
    Their iteration variables stay apart from the scope's own variables of the same name; one
    that a scope nested in the comprehension reads is a cell of the enclosing scope, named
    `.comp<n>_<name>` in `captured_vars` and in `var_names` of the values bound to it. Before
    3.12 a comprehension is a function of its own.
  - **3.14+ PEP 649**: annotations without `from __future__ import annotations` are evaluated
    lazily in `__annotate__` scopes.
  - **3.15+ PEP 810 (checked on CPython 3.15.0)**: `lazy import m` / `lazy from m import x`
    (`Import(is_lazy=1)` / `ImportFrom(is_lazy=1)`) are recognized and modeled as bindings
    materialized when the name is first evaluated, not when the module loads. The module
    body's `__lazy_modules__` makes the later imports of the modules it lists lazy as well
    (`import a.b` is listed as `a.b`, `from a.b import c` as `a.b`), in the module body only,
    outside the body and the `except` clauses of a `try` statement. heki reads the list only
    when the module body binds it once, at its top level, to a list, tuple, or set of string
    literals that nothing else in the module reads or changes; otherwise, for a relative
    import (whose absolute name the semantic model does not know), and in the `else` and
    `finally` clauses of a `try` statement, where CPython 3.15 makes some imports lazy and not
    others, whether the import is lazy is open (`ModuleImport::lazy_open`).
- **Bidirectional comparison with CPython `symtable` (quality assurance)**:
  - Following datamodel-code-generator's `scripts/_source_model.py:verify()`, the scope tree
    (kind, name, line, and parent), bound names and parameters, `global` / `nonlocal`
    declarations, and the kind of every reference (local / enclosing / global / builtin) are
    compared automatically with the real `symtable` of CPython
    3.11–3.15, which keeps the divergence from the language at zero
    (`tests/symtable_verify/verify_symtable.py`).

### 1.2 `IrFunction`
One `IrFunction` is built per function (`def`, `async def`, lambda, generator expression, a
comprehension before 3.12), per class body, and for the module's top level. The methods of a
nested class (its `__init__` in particular) are kept apart from the methods of the outer class
(against the `MUT101` false positives found in Stage 0-A). Qualnames are Python's
`__qualname__`: a class defined in a function is `factory.<locals>.Worker`, so its methods never
share a summary with a module-level `Worker`. A `class` or `def` statement in a function or a
class body that declares its name `global` binds the module's name, and CPython names it as a
module-level definition (`Worker`, `Worker.run`), as heki does; it is no method of the class body
holding it. Since such a qualname does not name the functions around the definition,
`IrFunction::enclosing` records the scope whose code makes each one, and the closure cells a
scope reads belong to the functions along that chain (`ModuleIr::enclosing_functions`).

Names follow Python's private name mangling (`heki_semantic::mangling`). Inside a class, at any
depth of nested functions, an attribute name that starts with two underscores and does not end
with two is recorded as the member it is (`self.__run` is `AttrLoad { attr: "_Parser__run" }` in
class `Parser`), and so is every name of a class body (`def __run` binds `_Parser__run`). A
function's own variables keep their spelling: they are no members. A definition keeps its
written name in its qualname (`Parser.__run`), so the declarations find a method reached by its
member name through that spelling (`ModuleDecls::definition_path`), and record what a class
holds by name (its members, properties, and cached properties) under the member name
(`mangling::member_name`). The contract patterns match a definition under both spellings
(`def_spellings`), an attribute target `C.__x` the member `_C__x` of the classes it names, and
a target without a class `__x` the member of any class.

```text
IrFunction {
    id, name, qualname,
    owner_class, owner_class_qualname,   // nested classes kept apart from their outer class
    kind:               Sync | Async | Generator | AsyncGenerator,
    params:             Vec<ParamInfo>,  // name, literal default, keyword-only, *args / **kwargs, PEP 484 annotation (Advisory lane only)
    return_annotation:  Option<String>,  // Advisory lane only
    decorators:         Vec<String>,     // @cached_property, @lru_cache, @property, CLI and web routes, lifespan, ...
    local_annotations:  Vec<LocalAnnotation>, // annotated locals and self.attr (Advisory lane only)
    var_names:          Vec<Option<String>>,  // source name of every SSA variable
    captured_vars:      Vec<String>,     // free variables captured by nested functions, lambdas, generator expressions
    blocks:             Vec<BasicBlock>, // can be ordered in reverse postorder
    loops:              Vec<NaturalLoop>,// header, preheader, body, exits, back edges, LoopBound, depth
    loop_carried:       Vec<LoopCarriedInfo>, // loop-header phis and whether they accumulate
    a1_hazards:         Vec<A1Hazard>,   // eval / exec / locals() / sys._getframe ..., with spans
    span, is_module_body, is_class_body,
}

BasicBlock {
    id, phis: Vec<PhiNode>,
    insts:      Vec<Inst>,               // every instruction keeps its byte span in the source
    terminator: Jump | Branch | Return | Raise | Unreachable,
    exc_target: Option<BlockId>,         // where an exception raised in this block goes (set by
                                         // its first instruction that may raise, or its `raise`)
}
```

### 1.3 Instruction set
Compound expressions are flattened into **basic instructions in explicit left-to-right order**.

| Category | Instructions | Notes |
|---|---|---|
| Constants and allocation | `Const`, `AllocBuiltin(kind, elems)`, `DictLiteral(entries)` | `list` / `dict` / `set` / `tuple` / `str` / `bytes` / number literals, comprehensions, unshadowed builtin constructors; `DictLiteral` keeps the order of `**` unpacking (`{**d, ...}` vs `{**other, **d}`) |
| Names | `LoadGlobal`, `LoadName`, `LoadDeref`, `StoreGlobal`, `StoreDeref`, `Copy`, `Delete`, `Import` | `Copy` adds to the direct alias set, `Delete` removes by strong update and leaves the name possibly unbound (a `LoadName` is its value until it is bound again, also at the end of an `except E as e:` handler; `Delete.global` marks a module global); `Import` / `LoadGlobal` carry the PEP 810 `is_lazy` flag |
| Operators | `BinOp(op, lhs, rhs, in_place)`, `UnaryOp`, `Compare(op, lhs, rhs)` | Augmented assignment is `BinOp` with `in_place`; `in` / `not in` are `Compare`; a chained comparison `a < b < c` branches after each link like `a < b and b < c`, with `b` evaluated once and each `Compare` spanning its own operands; until types are exact, any special method may run |
| Strings | `Convert(value, conversion)`, `Format(value, spec)`, `BuildString(parts)` | As CPython compiles an f-string: each replacement field, left to right, evaluates its value, converts it (`!s` / `!r` / `!a`, and `{x=}` without a specification: `Convert`, which runs `__str__` / `__repr__`), evaluates its format specification (an f-string of its own), and formats the result (`Format`: `format(value, spec)`, which runs `__format__`); each runs code of the value's class unless that is a builtin, and may raise. A field alone is the f-string's result (`__format__` may return a `str` subclass); any other f-string is a `BuildString` of its formatted fields, a new `str` that keeps none of them. A template string keeps its values unconverted and unformatted, as the tuple `Template.values`, formats its specifications' fields, and is otherwise opaque |
| Attributes and items | `AttrLoad`, `AttrStore`, `AttrDelete`, `SubscriptLoad`, `SubscriptStore`, `SubscriptDelete` | Stores make the stored value `Shared`; `AttrDelete` recognizes `@cached_property` invalidation (`del self.prop`) and `MUT201` writes; subscripts keep constant keys and slice bounds; the slice object itself is the tuple `(lower, upper, step)` of its bounds (`None` where one is left out), since `__getitem__` may keep it |
| Calls | `Call(callee, args, kwargs, is_awaited)`, `MethodCall(recv, method, args, kwargs, is_awaited)` | `is_awaited` separates awaited calls from synchronous ones (a coroutine awaited after it is made runs at its `await`, which the effect summary follows); `star_from` is the index of the first `*` argument, so arguments bind to parameters exactly up to it |
| ContainerIteration and generators | `MakeIter(iterable)`, `IterNext(iter)`, `MakeGenExpr(outer_iter, captured)`, `MakeClosure(captured)` | `MakeIter` and `MakeGenExpr` bind a **derived reference** of `iterable` / `outer_iter` to an explicit SSA variable; `MakeClosure` holds every value the function or class object keeps: its cells (free variables of nested scopes at any depth, the iteration variables of inlined comprehensions, the names read by lazily evaluated annotation and type-parameter scopes), its defaults, eagerly evaluated annotations, and a class's bases and keywords (`ModuleIr.class_bases` lists the bases among them, `ModuleIr.statement_uses` the defaults and keywords; a `type` alias is a `MakeClosure` without a lowered body) |
| Suspension | `Await`, `Yield`, `YieldFrom` | `Await` lets other tasks run (`Unique` stays unique), as do the implicit awaits of `async for` and `async with` (`InstKind::suspends`); `Yield` makes the yielded value `Shared` |
| Context managers | `EnterWith(mgr, is_async)`, `ExitWith(mgr, is_async)` | Mark the region of a `with` / `async with` (the lock and transaction regions of `EFF106`) |
| Exceptions | `CatchException(types)` | Starts an exception handler |

### 1.4 Lowering, fallbacks, and `LoopBound`
- **`for` / `async for` headers**: for `for x in expr:`, **`expr` (`for.iter`) and `it =
  MakeIter(expr)` are placed in the block before the loop (loop depth `d`)**; the loop body
  (depth `d + 1`) starts with `IterNext(it)` (this removed, by construction, the 7 Stage 0-A
  false positives of `EFF102` / `EFF103` that took the one-time `for.iter` call for a call in the
  loop).
- **Unreachable code** (after a `return`, `break`, `continue`, or `raise`, or after a `try`
  every path of which leaves) is lowered into blocks without predecessors that end in
  `Unreachable`: no merge, back edge, or handler takes a value from it. A loop there has no
  entry edge: the SSA lookup of a name gives a phi to a block with one predecessor that it
  reaches again, as to a join, so its walk around such a cycle ends. Its phis stay even when
  they merge one value besides themselves (only a block a path reaches drops such a phi for that
  value): a value the loop computes from itself (`d = d.x`) is defined through the phi, so every
  cycle of definitions passes through a phi, which the walks along definitions rely on.
- **Exception edges carry the bindings of the point that raised**: inside a `try` or `with`
  body a name is never rebound after an instruction that may raise in the same block
  (`InstKind::may_raise`; the binding starts a new block), so a handler's phi merges the
  bindings each raising point had; the Soundness lane's liveness takes a handler's needs as
  live before each such instruction. A module-level read of a `lazy import`ed name loads the
  global, since it may import the module there and raise (PEP 810).
- **Leaving `with` and `try`**: a `return`, `break`, or `continue` exits each `with` it leaves
  (`ExitWith`) and runs each `finally` body on the way, innermost first; a `finally` body is
  lowered once per way out of its `try` (normal completion, an exception, which it re-raises
  afterwards, and each jump), as CPython compiles it, and the Soundness lane rewrites a
  statement in it only if every copy is proven safe. The copies of a `finally` body nested in
  another `finally` body, or holding a way out itself, multiply with each copy around them. A
  typed `except` clause's test is opaque (a bare `except:` always matches); every `except*`
  clause may run on part of the group; an exception no clause takes is raised again.
- **Size budget**: a function's lowering takes at most 4 units per byte of its source (at
  least 4,096, at most 50,000; a unit is a block, an instruction, a phi, a name's value in a
  block, or a block a name's lookup passes; real code takes under one per byte). Past it,
  the function is unmodeled: its body becomes one call of an unknown callable with every
  parameter, whose result it returns, and its `a1_hazards` hold `unmodeled`, so the Soundness
  lane skips it and the Advisory lane sees one unknown call.
- **Unpacking** (`a, *rest, b = value`, also in `for` and `with` targets): every item is read
  (`SubscriptLoad` of a constant index at the target's position, negative after a star) before
  any target is written; `*rest` binds a new list.
- **`match`**: each case tests its pattern (PEP 634) and goes to the next case when a test
  fails: a value pattern is a `Compare` (`==`, or `is` for `None` / `True` / `False`), and the
  shape tests of sequence, mapping, and class patterns are an opaque `Const` (both outcomes
  possible); only wildcard and capture patterns always match. Captures bind the matched
  component once the whole pattern matched: an item (`SubscriptLoad`, negative after a star), a
  mapping value, an attribute (a positional class argument reads `__match_args__[i]`), a new
  list for `*rest`, a new dict for `**rest`; an or-pattern merges its alternatives' bindings.
- **Comprehensions and generator expressions**: when
  `RuntimeProfile.pep709_inlined_comprehensions` is true (3.12+), comprehensions are inlined into
  the enclosing frame; generator expressions are separate functions created by `MakeGenExpr`.
  An element is `append` / `add` / an item store on the accumulator, or a `Yield`; an
  unpacking element (3.15 PEP 798: `[*xs for ...]`, `{**m for ...}`, `(*xs for ...)`) is
  `extend` / `update`, or a `YieldFrom`.
- **A1 hazards** are references to a frame-introspection function, not only calls of it: a
  read of `eval` / `exec` / `locals` / `vars` (the builtin, or a name an import binds to one,
  in the scope the read resolves to), of `sys._getframe` and the like (through `import m as
  alias` too), and of a module global the module body binds to one (`snapshot = locals`) are
  hazards of the function that reads them, since whatever calls the value runs it in that
  frame (`map(eval, codes)`); `vars(x)` with an argument reads no frame.
- **A1 fallback**: a function with `a1_hazards` is excluded **from `heki_soundness` only**;
  **`heki_effect` (family 1 `MUT`, family 2 `EFF` / `DATA`) continues**.
- **Proof-state budget**: `heki_soundness` keeps a fact of each variable at each block, so a
  function's state is its blocks times its variables. A function whose product passes 8 cells
  per byte of its source (at least 4,096, at most 250,000) is not analyzed and gets no `OPT`
  candidate; no function with a candidate in the corpora passes it.
- **`LoopBound`** (never a Soundness premise; used for profitability and Advisory noise only):
  - `ConstSmall(k)`: a `for` over a small display (below) or `range(...)` (the builtin, by
    name resolution) spanning at most 16 (`SMALL_MAX`), also through `enumerate`, `zip`, `reversed`, `sorted`, `map`,
    `filter`, and `.items()` / `.keys()` / `.values()`, and with range bounds that are constant
    integer expressions (`+`, `-`, `*`, module constants included); or over an iterable that a
    `len(x) <= N` test on every path to the loop caps at `N <= 16` (`if not 1 <= len(x) <= 6:
    raise`, `assert len(x) <= 3`, the exit of `while len(x) > 3: x.pop()`), when nothing in a
    block that reaches the loop grows `x` or lets it escape (returning it after the loop is
    fine). `FnIndex::guarded_len` reads the outcome of the test on the edge into a block whose
    one predecessor branches on it, through `not`, `and` / `or`, and comparison chains, and the
    tests on the single-predecessor way to each operand. "Every path" is that of the path facts
    (`Paths::block_dominators`): a branch the module's constants decide is not taken, and a
    block raises into its handler only where an instruction of it may raise. A generator expression's or a
    comprehension function's loop over `.0` takes the bound of the iterable its creator passes
    (`IrFunction::passed_from`); and a `while` loop over a counter that runs at most 16 times
    (`FnIndex::counted_trips`): its test compares a header phi with a constant (`==`, `!=`,
    `<`, `<=`, `>`, `>=`, either side) or tests the phi itself (a count down to zero), the
    test's false side leaves the loop (an operand of `and` bounds it, one of `or` does not), the
    phi enters the loop as an integer constant, and every input along a path back is the phi
    plus one and the same nonzero constant (`i += 1`), so `i = 1` ... `while i < 6:` runs 5
    times.
  - `RetryPattern`: a retry body (a `try` that leaves the loop on success and sleeps in a
    handler) over a small constant range or a target named like `retry` / `attempt` (also one
    name of a tuple target), or a `while` loop with a retry body that sleeps; and a paced loop
    other than `while True:`: a `while` loop, or a `for` loop whose body never reads the names
    its target binds (a target binding an attribute or an item counts as read), in which every
    path from the header back to it runs a call that waits
    (`facts::waits_every_iteration`): a sleep (a call or method named `sleep`) of a duration
    other than a literal zero, or a `wait` with a `timeout=` keyword that is not a literal zero
    or a positive constant (`stop.wait(1)`; `asyncio.wait(tasks)` may return at once), awaited
    in a coroutine (`asyncio.create_task(asyncio.sleep(1))` waits elsewhere). The paths are
    those of the path facts (`Paths::on_every_iteration_passes_one_of`, the wait to its
    return): a branch the module's constants decide is not taken, and an exception raised
    before the wait returns skips it.
  - `DaemonPolling`: `while self._running:` / `while not self._stopped:`-style flag loops (the
    flag's name has the word `running`, `active`, or `alive`, or after `not` a word like `stop`
    or `shutdown`; an event's `is_set()` / `wait(...)` tests the event as the flag; and the body
    does not drain it as a collection: no `pop()` / `popleft()` / `remove()` ..., subscript, or
    `for` over it), `while True:` with a retry body that sleeps, and a paced `while True:` loop
    (resident workers and reconnect loops).
  - `Unbounded`: everything else.
- **Size facts** (`heki_ir::facts`, the one home of size and trip-count facts; profitability and
  Advisory noise only, never Soundness premises):
  - A **small display** (`SmallDisplay`) holds at most 16 items (`SMALL_MAX`) and nothing
    grows it past that or lets it escape: a list, tuple, set, or dict display without `*` /
    `**` unpacking, a string constant, a builtin constructor (`list`, `tuple`, `set`,
    `frozenset`, `dict`, `sorted`) of an iterable the code fixes, a slice with constant bounds
    (of a container of unknown kind it has a size but no kind, so it bounds loops and scans but
    is no module or attribute display), or what a comprehension function returns
    (`ModuleConstants::comprehensions`, Python 3.11). Shrinking it and passing it to read-only
    builtins are fine; a store at an unknown index (`xs[k] = v`, possibly a slice) counts as
    growth. A new object may gain items a fixed number of times: an `append` / `add` /
    `insert` / `setdefault`, or an item store into a dict, adds one per run of its block, and
    `extend(x)` / `update(x)` as many as `x` yields; a block runs once per iteration of each
    loop around it that the object outlives (it is made outside the loop, or a header phi
    carries it), so a comprehension or a list filled in a fixed loop is small. A module
    constant's object gains none (every call shares it). Tuples and strings are fixed whatever
    their uses, also in other modules, unless another module rebinds or deletes the name
    (`ModuleDecls.rebound_globals`, `ProjectDecls::changed_elsewhere`).
  - The builtins these facts read (`range`, `len`, `sorted`, `min`, `enumerate`, ...) count only
    where a load of the name finds the real one (`ModuleConstants::trusts_builtin`, which
    `ModuleBindings` reads too): not in a module that stores or deletes the name, or that binds
    names behind the IR (`exec`, a change through the dict `globals()` returns), and none in a
    module with a star import, which may bind the name of any builtin, since heki does not read
    which names the imported module exports, also when it is a project module. The trust is per
    module, as `ModuleBindings` and the Soundness lane's builtin scope take it: a function may
    run after the import wherever the import stands. A size resting on such a builtin is then
    not known, so no claim rests on it (`range(len(rows))` gives `EFF103` no origin), and the
    rules that read an unknown size as data-sized report the loop (`range(3)`).
  - `NaturalLoop.len_caps` (`LenCap`) records `while len(x) < N` caps up to 4096
    (`LEN_CAP_MAX`); `LoopCarriedInfo.updates` lists the real updates of a loop-carried value.
  - `IrFunction.starred` marks starred display elements, `ModuleIr.class_bases` the bases a
    class statement passes, `ModuleIr.statement_uses` the values a statement uses without an
    instruction reading them (a decorator, a function's or lambda's default, a class statement's
    keyword, the class of a class pattern, the types of an `except` clause, the cause of
    `raise ... from`, each with its expression's span), `ModuleIr.annotations` the spans of the
    annotations a function evaluates eagerly, `BasicBlock.term_span` the source of a block's terminator (blocker
    lines point at it), and `ModuleIr.constants` (`ModuleConstants`) the module's constant
    integers, booleans, floats, displays, strings, and tuples of strings. A string constant, or
    a tuple of them, is one entry: its text (`strs` / `str_tuples`, one `Arc<str>` per string
    the module writes, which aliases and items share), whose length gives its display. It is
    the one view of a module name bound once (a star import that runs, not one under
    `if TYPE_CHECKING:`, rebinds every name not bound after it for good, also one with an
    underscore, which the imported module's `__all__` may list; `LastBindings`): the
    effect lane's declarations read their number, boolean, and string constants from it, and `GlobalStores` is the one count of the stores
    and deletions of each module global that it, `ModuleBindings`, and the declarations read.
  - The Soundness lane reads these facts within one module only (`FnIndex`). The Advisory rules
    read other modules' small displays through the declarations (`ModuleDecls.small_globals`,
    `ClassDecl.attr_displays`), so the language server re-checks their users when they change.
    A module display another module may grow or rebinds (`cfg.QUEUE.extend(more)`, `from cfg
    import QUEUE` and `QUEUE.append(x)`, `cfg.QUEUE = []`; `ModuleDecls.grown_globals`, by the
    shared `FnIndex::grown_vars`) is not fixed for the Advisory rules, in its own module or
    any other (`ProjectDecls::grown_elsewhere`, a declaration key the language server tracks).
  - `FnIndex::fixed_trips` gives the most times a loop runs at any count, as a `Trips`:
    `Fixed(n)` when the code fixes the count whatever the input (`range(1000)`, a tuple display or
    a list or set display nothing grows, a module's string and string-tuple constants, a length
    test that pins the length, `len(x) == 40` or `!= 40` leaving, and a counted `while` loop), and
    `Capped(n)` when a constant only caps what may be input (a length cap such as `if len(x) >
    1000: raise`, a slice `rows[:100]`, a `zip` with an iterable the code does not bound). A cap
    is an upper bound, not a fixed count. A length test bounds the loop when it measures the
    iterable or a read of the same access path from the same root (`len(report.get("runs", []))`
    then `report["runs"]`; a `get` default counts when the test rejects its length), with nothing
    on the way storing into the root, deleting from it, or passing it to a call.
    `LoopBound::ConstSmall` stays the view of counts up to `SMALL_MAX`. No claim rests on a size:
    `Open` only leaves the loop data-sized to the rules that read it so.
- **Path facts** (`heki_ir::facts::Paths`, Advisory and Profitability only), computed on demand
  for the rules that read them, over the walk facility's `FnShape` and the owner's `Budget`:
  - `FnIndex::decided` (`Decided`): a branch whose test the code fixes never takes its other
    side, and code only that side reaches is dead for every fact. The tests are literals, module
    constants (`ModuleConstants::ints`, `strs`, `bools`, and `typing.TYPE_CHECKING`, which is
    false whenever the code runs; a relative import, `from .typing import TYPE_CHECKING`, names
    the package's own module and fixes nothing), `not`, `==` / `!=` / `is` of two such values, and the phis of
    `and` / `or` along the edges that can run (one pass in reverse postorder: a phi's input along
    a back edge leaves it undecided).
  - Exception edges: an instruction raises into its block's handler unless the assumptions every
    exact claim states rule it out (`PathReader::raises`): bindings are static, imports resolve
    as configured, the catalog describes the library, and no asynchronous or resource-exhaustion
    exception arises (`KeyboardInterrupt`, `MemoryError`). A load of a module name the module
    body binds once on every path (`ModuleBindings`) or of a real builtin does not raise, nor does
    an attribute load of a member of a module a plain `import` binds once (`from . import json`
    binds the package's own module, `InstKind::Import::relative`), nor a list or tuple
    display without unpacking. Attribute loads of other objects (unless the reader finds the
    attribute on the object's class, `PathReader::attribute_found`: a catalog method, or a
    project method no decorator replaces, where no class the object may be under R2, a project
    subclass and the other bases it derives from, binds the name to other code, defines
    `__getattribute__`, or has a metaclass heki does not know, `ProjectDecls::attr_read`),
    subscripts, set and dict displays, imports, calls, `await`s, and operators do. A method
    call makes no call until its lookup of the method returns, so where that lookup may raise,
    the call counts as made only after it. A handler's type is not matched. A
    `with` body's exception goes on past the `with` (its `__exit__` may suppress it) unless the
    reader establishes that no manager of the statement suppresses (`PathReader::exit_suppresses`).
  - `Paths::on_every_return` (a point runs whenever the function returns normally) and
    `Paths::on_every_iteration` (on every iteration of a loop that reaches its back edge) answer
    `Must::{Every, NotEvery, NeverCompletes, Dead}`, for a call made (`Progress::Called`: its own
    exception still counts) or returned (`Progress::Returned`). Both are dominance in a graph of
    two nodes per block: its head, before its first raising instruction runs, holds the block's
    exception edge, and its tail its normal edges. The later raising instructions of a block go to
    the same handler and every path to them passes the first, so moving their edges to the head
    changes no dominator. The graph of a region is built once, its dominators found in at most
    four passes in reverse postorder; past the budget the answer is `Open`.
    `Paths::on_every_return_passes_one_of` and `Paths::on_every_iteration_passes_one_of` ask it
    of a set of points (every path that completes the region passes one of them, also when none
    completes it): a walk back from the region's end over the same graph, never entering a
    passed node, does not reach its root.
  - `ItemOrigins::of_loop` (`heki_ir::facts::origin`): where the items a `for` loop steps
    through come from, as a set of `ItemOrigin`s: a parameter (`Param`), an attribute or item of
    one (`ParamPart`), the method's receiver (`Receiver`), a collection the code fixes (`Fixed`),
    or the result of a call (`Call`), which the reader resolves against the catalog: a source is
    a catalog target, never a name. Items pass through `list`, `tuple`, `set`, `sorted`,
    `reversed`, `iter`, `filter`, `enumerate`, `zip`, slices, dict views, and a comprehension,
    generator expression, or list filled only by appends that produces a loop's item; through
    `map`, a comprehension's element expression, a part of the loop's item (`r[0]`), and
    `range(len(x))` only the count carries (`Carries::Count`). A set or dict of what carries only
    a count merges equal results and has no source. `str.split` / `str.splitlines` give pieces
    of their receiver's text (`ItemOrigin::text`). A phi takes the inputs along the edges that can run. An
    item of a call's result, a global that is no constant, and anything else are `Open`. The walk
    is a `Closure` over questions `(function, value, steps)`, the steps being what the way from
    the loop applies to the value's sources (an item, a count, pieces of text).
  - The reader has two set-ups. Where types are inferred, `heki_effect::with_paths` builds it
    (the module's bindings, members from the catalog or the project, methods of the inferred
    class, managers the catalog says suppress nothing): the summaries' must-facts, `EFF103`'s
    and `DATA101`'s every-iteration premise, and `heki dump-facts` read it. Inside `heki_ir`,
    where no types exist, `PathReader::ir()` knows nothing beyond the IR (every raising
    instruction raises, every `with` may suppress): the module's bindings
    (`ModuleBindings::of`), the length caps on every path to a loop
    (`Paths::block_dominators`), the paced loops (`Paths::on_every_iteration_passes_one_of`),
    and the appends `ItemOrigins` accepts as filling a list read it. It answers `Every` in fewer
    cases, so each of these facts holds in fewer programs than it would with types.
  - `heki dump-facts` (hidden) prints these facts and `fixed_trips` as JSON for the tests.

---

## 2. Advisory / Effect lane (`heki_effect`): families 1 and 2

### 2.1 Light receiver type resolution (for the call graph only)
No type errors are reported; resolution only finds call targets, the classes of attributes, and
the targets of write and read boundaries.

| Priority | Source | Confidence | Example |
|---|---|---|---|
| 1 | Local constructors and direct imports (relative imports, source roots, re-exports) | Exact | `requests.get(u)`, `client = httpx.Client(); client.get(u)` |
| 2 | PEP 484 annotations (parameters, locals, return types, attributes), container item types, and return annotations along the MRO; `isinstance` narrowing and unions of up to three classes | Typed | `def f(repo: UserRepo, db: Session): db.execute(...)`, `for m in models: m.save()` with `models: list[Model]` |
| 3 | `self.attr = Cls(...)` in `__init__` or `__post_init__`, and module globals the module body binds, each the join of what every store of it binds | Typed | `self.http = httpx.Client()`, then `self.http.get(...)` in another method |
| 4 | Same-name methods in the class hierarchy (CHA / RTA); a project `Protocol` dispatches to its structural implementations (a call site keeps the protocol's own declaration of the method, `CallSite::declared`, which the read and call contracts judge it by) | Heuristic | Concrete subclasses of an abstract base (more than 8 candidates: unresolved) |

A module global the module body binds to a value of known type, and an instance attribute
`__init__` or `__post_init__` binds to one through `self`, have the join of what every store of
the name binds (Priority 3; `ModuleDecls::global_types`, `ClassDecl::attr_stores`,
`resolver::stores`).

- **What a store stores into.** A store stores into what its receiver holds: `self` in a method
  (a copy of it, the closure cell of a function nested in the method, `super()`), or a value whose
  type the inference knows: an instance of a project class (`inst = cls()` in a class method, a
  parameter `obj: Helper`, a union of such classes) or a project module (an import in a function
  too). A global's stores are also the module body's and those of every function declaring it
  `global`. An attribute's type joins the stores into the instances of every class whose code may
  store into an instance (the class, its project subclasses, and the project classes along their
  MROs; `ProjectDecls::member_type`).
- **The stores.** `obj.attr = v`, `setattr(obj, "attr", v)`, `object.__setattr__(obj, "attr", v)`,
  `super().__setattr__("attr", v)`, the same calls through an alias (`_set = object.__setattr__`),
  and writes of the namespace (`obj.__dict__["attr"] = v`, `vars(obj).setdefault("attr", v)`).
  The name may be a literal or a string constant of a module or a class (`KEY: Final`), or one
  of the string constants of the tuple, list, or set display a loop takes it from
  (`for key in ("a", "b")`), which stores each (`stores::key_name`, the one decision of the
  names these stores and the ownership facts read).
- **What a store binds.** A store of `None` adds nothing (a call on it raises). A store of a value
  of unknown type, or values no tracked type covers, leaves the name untyped. `obj.attr += v`
  binds what the operator gives on the attribute's own value: nothing new when the name holds a
  list, set, dict, deque, `bytearray`, `str`, or `bytes`, and an unknown value otherwise.
- **Stores heki cannot name.** `setattr(obj, name, v)`, `obj.__dict__.update(other)`,
  `obj.__dict__ = state`, `obj.__class__ = C` (also through `setattr` or
  `object.__setattr__(obj, "__dict__", state)`), and a namespace handed on store any attribute
  (`ClassDecl::stores_any_attr`), except in `__setstate__` from its state parameter
  (`ClassDecl::restores_state`): those restore what an instance held, while no project code calls
  `__setstate__` itself, reads the method, or names it (`getattr(obj, "__setstate__")`) outside a
  `__setstate__` handing its state on, and the family has no
  `__reduce__` / `__reduce_ex__` and no `__getstate__` building the state from anything but the
  instance's own attributes (`FunctionDecl::state_of_self`, which holds only when every
  definition of the method does).
  A store under a key of the namespace of an instance of the class or a subclass
  (`for key, value in other.__dict__.items()`, `vars(other)`, their keys, a `list`, `tuple`,
  `sorted`, or copy of them, `update(other.__dict__)`), or under a field the class or a
  subclass declares as the catalog's `[[declared_fields]]` keep them (the keys of
  `type(other).model_fields`, the `name` of each of `dataclasses.fields(other)`), binds any
  value too, but adds no attribute an instance did not have or the class does not declare: only
  the others set `ClassDecl::adds_unnamed_attrs`, which the unmatched contract entries read.
- **Globals the module may rebind.** In a module whose globals `globals()` or `exec` may change,
  every store of a global binds a value of unknown type. A module-body store does too when a star
  import that runs (not one under `if TYPE_CHECKING:`) may bind the name after it: the store comes
  before the import, or on only some of the paths after it (a branch, an `except` clause, a loop;
  `heki_ir::LastBindings`). A star import of a project module whose `__all__` fixes the names it
  exports binds only those (`ModuleDecls::exports`): the module body binds it once, on every path
  that finishes the body, to a tuple of string literals or to a list of them that neither the
  body (`__all__.append(name)`) nor a function changes.
- **Other modules.** What other project modules store into a module's globals and into the
  instances of its classes (`ModuleDecls::stores_elsewhere`) is joined where the type is read. A
  store's receiver is typed as the storing module's decided types type it: one an attribute or a
  global holds (`self.base.client = v`) is typed in each visit of the fixpoint below, and when
  the first round's types hold, the functions of the fixpoint are classified once more under
  them.
- **Stores reading names being decided.** A store reading another such name
  (`self.active = self.client`, `self.items = self.items[1:]`) binds a value whose type depends on
  the one being decided. The types are a least fixpoint (`heki_ir::walk::fixpoint`): each read
  starts with no value, then gives what the stores bound so far give, as the final declarations
  will (the family join); a function is visited again only when a name it reads grew, at most
  four times, and inferred again only when a read it made gives something else: a visit first
  asks the reads of the function's last inference (the first round's on the first visit), and
  when each gives what it gave, the function stores what it stored then. The fixpoint has a
  budget of 64 steps per IR unit of its functions: a visit takes one, one per read it asks
  again, and the size of what the function stores. A read the final declarations answer from
  another module (an attribute of a class with a family member in another module, a class of
  another module along the MRO) gives what the declarations of pass 1 give. Past the visit limit
  or the budget, the first round's types hold, with those reads untyped. Where the types differ from the first round's joined with them, the latter are kept
  (`ModuleDecls::store_fallbacks`), with what each function of the fixpoint stores and reads, once
  per function (`StoreFallbacks::dependencies`). A name's fallback holds once another module
  stores into a name a function storing it reads, or into a name such a function reads through
  the stores of the names it reads; one walk over the functions finds them
  (`ProjectDecls::elsewhere`).
- **Changes in place.** A change the module's code makes to the object a global or an
  attribute holds, through a load of it (an item store `HANDLERS[kind] = run`, a method that may
  add items such as `append`, `update`, or `setdefault`, an in-place operator `ITEMS += more`;
  `heki_ir::FnIndex::changes_in_place`, through copies and phis), is a store of the loaded type
  joined with what the changes add (`types::Container::added`, `Ty::with_added`; of an
  annotated global's declaration). A list or a deque gets the items `append`, `insert`,
  `appendleft`, `extend`, `extendleft`, `+=`, and item stores add (of a slice, the value's
  items), a set those of `add`, `update`, `symmetric_difference_update`, `|=`, and `^=`, a dict
  the keys and values of item stores, `setdefault`, `update` (also by keyword), and `|=`. An
  argument of unknown type, an unpacked one, a method the container does not have, and a change
  of what a change gave (`d.setdefault(k, []).append(x)`, `xs += ys` then `xs.append(x)`) add
  items of unknown type: a name holding `{"noop": noop}` that `HANDLERS[kind] = run` changes
  with `run` of unknown type keeps `dict` and no item type, and `HANDLERS["require"] = require`
  gives its values the type of either function. Any other class keeps no items. A change
  through the module's own object (a module importing itself, `tables.HANDLERS[kind] = run`),
  whose read types the global by its name alone, leaves the global its declaration or no type.
  Another module's change records what it adds (`StoredElsewhere::changed`, typed against pass
  1), joined into the name's type where it is read. A local display joins what the function's
  changes add the same way (`infer::Engine::add_contents`; a method a display does not have
  raises and adds nothing), and an in-place operator on a container of known class gives its
  type with what the operator adds. What a call the object
  is handed to does is not followed (the assumption on objects of unknown origin), nor a change
  of an item it holds (`d[k].append(x)`).
- **Annotations.** An annotation types every store of the name it declares (Priority 2): an
  annotated global holds its declaration, refined by what the value says beyond it
  (`ProjectDecls::refine`: a narrower value, or for an instance of the declared container class
  the item slots where the value is narrower or the declaration says nothing, such as the
  functions of `HANDLERS: dict[str, Callable[..., None]] = {...}`), an annotated attribute its
  declaration.

Stores of two or three classes give a union, on which a call dispatches as on any union (one of
`httpx.Client` and `httpx.AsyncClient` leaves `get` unresolved). A store through a receiver
whose type the inference does not know (`def configure(obj): obj.client = ...`) is not one of
them, nor is one through a receiver read from an attribute or a global of another module
(declarations of pass 1 type neither while a module's stores are decided), nor one into a class
that only another module's store into the receiver adds (docs/scope.md, "Known limitations").

A class's bases (`ClassDecl::bases`) are the values its class statement passes, named where the
statement runs. A module-level statement names them through the module's names: an import, a
module-level class or alias, an attribute of an imported module. A statement in a function or a
class body takes each value from the IR (`ModuleIr::class_base_values`) through copies and the
origin of a generic alias (`Base[int]`): a name the function loads from the module (in a class
body also one the body has not bound yet, which Python looks up in the globals), what an import
in the function binds, or an attribute of either, so a local alias of the import is the import.
Any other value is an unknown base (`is_unknown_base`): a parameter, a local bound to anything
else, a closure cell, a class-body name bound to anything else, a value merged from paths that
disagree (also the copies of a `finally` body), or a call's result; so are an expression or a
starred base, a module variable heki names no class for (`Base = declarative_base()`), and every
base of a statement in a function too large to model. An unknown base keeps its place in the MRO
as a base outside the project that heki knows nothing of: it may define any method or attribute
and its metaclass is unknown (`ClassDecl::custom_metaclass`), and no class derives from it, so
the class is in no project class's subclass family. Each unknown base is a class of its own,
named after its class statement and its place (`<unknown pkg.mod.Left 0>`): two of them never
order each other in a C3 linearization, as two distinct classes do not. The declarations are built before inference,
which reads them, so they follow the IR definition only; base references (below) type the same
values with inference, which also follows closure cells and class-body aliases.

A method called on an instance is the definition the class's MRO (Python's C3 linearization of
its project bases) finds first, plus the overrides of its project subclasses (Priority 4). A
named class is exactly that class: constructing it runs the `__init__` its MRO finds, and
`Base.method(obj)` the `method` it finds; `cls` in a class method may be any subclass, so
`cls()` and `cls.method()` dispatch like a call on an instance, and so do `type(obj)` of an
instance of a project class that is not a protocol (`obj.__class__` is not: a class or a
subclass may rebind it) and a value annotated `type[C]` or `typing.Type[C]` (`Ty::Class`; the
class object of a protocol, of `Any`, or of one of several classes is of no known class). A call
of a property's value (`obj.prop(...)`, the getter found first along the MRO, with or without a
setter) runs the getter as reading `obj.prop` does and calls the value it gives, typed by the
getter's return annotation (`self.data_type(is_dict=True)` of a property returning
`type[DataType]` constructs `DataType`). A method's calls on its own
`self` run the overrides of every subclass, so a mixin's summary holds the effects of every
class sharing it; an execution effect reached only through the override of a subclass records
that class (`EffectSummary::receiver_effects`), and a call on a receiver of known class runs it
only when that class and the receiver's derive one from the other (an unknown receiver runs them
all). An async client sharing a command mixin with a sync one runs the async implementation.
Calling an `async def` that no decorator replaces gives a coroutine (`Ty::Coroutine`): its return
annotation is what awaiting it gives (`await`, and the target of `async with`), and awaiting a
future or a task gives an untracked result. The catalog types an external awaitable by what
awaiting it gives. A catalog API that runs a callable it is handed and hands back what the
callable returns (`result_of`: `asyncio.to_thread(f, *args, **kwargs)`,
`loop.run_in_executor(executor, f, *args)`, `anyio.to_thread.run_sync(f, *args)`,
`executor.submit(f, *args, **kwargs)`) returns the coroutine of that result, or the future its
`return_type` names holding it (`Args::Result`, which `await` and the future's `result()` give):
what the call `f(*args, **kwargs)` returns, with the arguments the API passes on, as certain as
that call written out. A call of a value the function makes is typed as the call written where
the value is made: a bound method (`m = obj.method`) as `obj.method(...)`, a lambda by the value
of its body (inferred as a child scope reading the function's cells), and a `functools.partial`
object as a call of its callable with the arguments the object holds first (`partial.rs`, shared
with the call targets).

A scope nested in a function (a comprehension or generator expression with a scope of its own, a
lambda, a nested function or class) reads the function's variables through their closure cells.
A cell holds every value the function binds to the variable, its parameter's and each
assignment's, so whenever the nested scope runs the cell has the join of their types (`self` in
a method has its class). A scope created in an inlined comprehension (3.12 and later) reads the
comprehension's iteration variable, a cell of the enclosing frame apart from the frame's own
variable of the name, which the scopes created outside the comprehension read. A cell that a
nested scope may rebind (`nonlocal`, an assignment expression in a comprehension scope before
3.12, any scope nested in a function too large to model) is of unknown type.

A cell that the function binding it binds only as one of its parameters, and that no nested
scope rebinds, is that parameter in every scope reading it (`FunctionDecl::captured_params`, one
list per qualname, which scopes sharing a qualname share): every scope nested in the function
gets one more parameter per such cell, past the parameters of every scope with its qualname, so
a scope that does not read it can still hand it to one that does (a generator expression calling
a nested function), and the masks of its facts (`may_params`, `obj_params`) name it. A call binds
it (`BoundCallee::params` past the scope's own) to the caller's own parameter when the caller
binds the cell, to the caller's position of it when the caller is nested in that function too,
and to nothing otherwise: a closure called away from the function. So the mapping of
masks across calls takes `self` in a comprehension, generator expression, lambda, or nested
function back to the method running it (a class's own initialization, `own-receiver`, the walk
up the callers), and the owner contracts' lexical attribution maps the positions of a lambda,
comprehension, or generator expression to the function holding it directly.

Module names come from the paths below a source root (`pkg/mod.py` → `pkg.mod`,
`pkg/__init__.py` → `pkg`), the deepest one holding the file, else the project root. The `src`
setting lists the source roots; without it they are detected as ty detects them: `src/`, the
directory named after the project when it holds one of the same name (`acme/acme/`), and
`python/`, each when it is not itself a package (no `__init__.py`), plus the members of the
project's uv, Hatch, and PDM workspaces: a member's own detected source roots, else its top-level
packages named below it (`packages/api/acme_api/` is `acme_api`; configuration.md, "Workspaces
and monorepos"). `src/acme/io.py` is
`acme.io`, for regular and namespace packages and for modules directly under `src/`. A module name that several files share is defined by one of them, in full and
incremental analysis alike: an implementation before its `.pyi` stub, a package
(`pkg/__init__.py`) before a module file of the same name (`pkg.py`, which Python does not import
next to it), then the greater path. A relative import resolves inside the packages it climbs; one
that climbs past the top-level package binds nothing (`ImportError`), except that a project root
holding `__init__.py` is itself the package of the top-level modules.

`from pkg import parse`, `import pkg.parse as p`, and `pkg.parse` read after `import pkg` take
the attribute `parse` of the package once `pkg/__init__.py` has run; Python imports the submodule
`pkg.parse` only when there is none, and its first import binds it on the package, replacing what
the package bound before. So when the project has the module `pkg.parse`, the name is decided by
the package's last binding of it (`ProjectDecls::from_import`): none, or an import of the
submodule itself (`from . import parse`), gives the submodule; an import or alias whose object is
read through the submodule (`from .parse import parse`, `parse = parse.parse` after
`from . import parse`), bound after the submodule was loaded, gives that object; any other binding
(`def parse`, `from .core import parse`) holds only until something first imports the submodule,
so the name is open (`Symbol::Unknown`); a name read on it (`parse.helper`) is still the
submodule's, which it is spelled like. `from pkg.parse import x` always takes the module.

A module-level name bound once to another object by name, besides the imports of the name that
binding always replaces (`_set = object.__setattr__`, `sleep = time.sleep`, `Alias = Node`,
`parse = parse.parse` after `from . import parse`), is that object, as a value
or in an annotation, in its module and imported from it (`ModuleDecls::aliases`,
`ProjectDecls::object_named`), unless another module rebinds or deletes it. Any other global a
module binds is the module's own object of unknown value (`Symbol::Unknown`), never the builtin
of the same name: calling it is unresolved, unless a `[[tool.heki.ecosystem]]` entry declares it
(`get = functools.partial(...)` listed as blocking, or with a `return-type`), which then
describes it as it describes a library symbol. A name under a project module that the module
does not bind (a compiled submodule, a star import of a module outside the project) is what the
catalog says of it.

A star import of a project module (`from ._utils import *`) binds the names that module lists in
its `__all__`, when the module binds `__all__` once to a list or tuple of string literals it does
not change (`ModuleDecls::exports`), and without `__all__` the public names (no leading
underscore) its namespace holds once its body has run, those its own star imports bind included
(`ModuleDecls::star_imports`, `ProjectDecls::star_binding`). A name the module binds no other way
is then the name read on the module the star import imports (`pkg.utils.fetch` is
`pkg.utils._utils.fetch`), in the module itself and read from other modules, as an import would
bind it: `canonical`, `object_named`, `from_import`, and the reads of constants follow it. A name
the module binds itself keeps that binding, and the existing rules on what a star import may
rebind (imports, constants, the globals the module body stores before it) still hold. The name is
open (`Symbol::Unknown`), not external, when a star import binds it through a module heki could
not read, through an `__all__` heki cannot read (built at run time, changed in place, imported),
or when star imports bind it to different objects, which a star import of a module outside the
project may do for any name (`try: from ._speedups import *` with a fallback). Deciding a name
reads each module once, however many paths of star imports reach it; a star import back into a
module being decided binds nothing more. It follows at most 128 star imports nested in one
another (`MAX_STAR_IMPORT_DEPTH`), and past them the name is open. A change another module makes
to the `__all__` of the module would have to run before the star import, and is not followed.

A class-body name bound last, outside a field, to a module-level name (a function or class of
its module the module binds once, an alias of its module, a name an import binds, or an attribute
of one) or to `staticmethod` of one (`factory = Scope`, `REQUIRE: ClassVar[...] =
staticmethod(require)`) is that object (`ClassDecl::named_attrs`, `ProjectDecls::class_alias`),
when no code of the module stores the attribute on the class or on an instance, no other module
rebinds it, and no hook runs on the read. The binding a name holds once its body has run is the
last one every path runs and each later one that runs on some paths only
(`heki_ir::LastBindings::last_of`): `def fetch(...)` then `fetch = _quiet` is `_quiet`, and
`if flag: fetch = _quiet` after the `def` is either the method or `_quiet`
(`NamedAttr::body_paths`), a call through it running both. Read through the class object
(`Holder.REQUIRE`, also inherited) it is the object itself; read on an instance, a function
outside `staticmethod` is a bound method, which binds the instance first, and anything else is
the object, joined over every class the instance may be as for a call below (a subclass finding
the name as anything else, or a bound method, leaves the value unknown). A call through `cls` or an instance runs the object of every class the receiver may
be (the class and its project subclasses, at most `DISPATCH_CLASS_LIMIT`); a class among them
finding the name as anything else leaves the call unresolved, and so do objects that bind a
receiver differently. A call of a class bound so constructs it (`self.factory(...)`).

A module-level function or class that the module body binds again, to named objects only, holds
in the same way what binds it last (`ModuleDecls::aliases`), or one of the objects the bindings
that may be last bind (`ModuleDecls::alias_paths`): a call through the name, in the module, from
another one, or imported, runs each of them. An alias of the definition bound before the
rebinding (`_original = fetch`) keeps the definition (`ProjectDecls::follow`).

A class or function whose statement a function or a class body runs under a `global` declaration
of its name (`global Config`, then `class Config(Base): ...` in `_define`) is a definition of the
module under that name (`ModuleDecls::made_in`), with its bases, methods, and attributes: contract
targets, annotations, imports, and subclasses in other modules name it as one defined at module
level. The module body binds it where it calls such a function, a plain module-level `def`
binding the name once on every path through it, or runs such a class body (`BodyBindings`), as it
binds a name by a store. Bound so on every path, the name holds the definition; when the module
body may not run that binding (no call, a call on a branch, a function nested in another), a read
of the name may find it unbound, so a call through it may run another object than the definition
(`ModuleDecls::rebound_definitions`); a later binding on some paths makes it one of the objects
(`ModuleDecls::alias_paths`). The code of the definition's class body is that function's for
`attribution = "lexical"`.

A value bound to one of several named objects (a branch `f = a if c else b`, the values of a
table) is one of them (`Ty::OneOf`, at most `MAX_NAMED`, eight): a call runs every one, which must
bind a receiver alike. A module global bound once, by the module body outside a loop, to a dict
display with string-literal keys and no `**` entry, some value of which is a function, a class,
or a partial object, and that no code changes in place (through its name or a module object,
the module's own included) or rebinds, is a table (`ModuleDecls::tables`, at most 256
entries): reading it with a key a string constant gives (`HANDLERS["create"]`,
`HANDLERS[KEY]`, in its module, imported, or through the module) gives that entry's value, a
later entry of the key replacing an earlier one; any other key, and iterating its values, give
one of its values.

A function every `return` of which gives what one import binds, an import in the function (`from
pkg import ops`, then `return ops`, which breaks an import cycle) or one of its module, with no
decorator but those keeping calls as defined and a `functools` cache, returns that object
(`FunctionDecl::returned_import`) when it is a project module or the function has no return
annotation of a class of its own (none, `-> ModuleType`). A value of
`types.ModuleType` says nothing of the names the module binds: a call of one is unresolved, as on
`Any`.

A module-level `TypeVar` bound once with a `bound=` (a name, an attribute of one, or the string of
an annotation) or constraints is that bound, or the union of the constraints, wherever an
annotation names it, in its module or imported (`ModuleDecls::type_vars`,
`ProjectDecls::declared_ty`): `for c in run.contexts` of `contexts: list[C]` with `C =
TypeVar("C", bound=View)` gives a `View`. An unbounded one gives no class.

A method that no project class of the receiver's MRO defines comes from the first base outside
the project, when heki takes it to define the method (a base it knows nothing of coming first, an
unknown base included, leaves the call unresolved): a special method (`__init__`, `__copy__`,
...), a public method of a builtin type other than `object` (`dict.update`), or a member Layer 2
lists (`pydantic.BaseModel.model_copy`). Otherwise the call is unresolved
(`abc.ABC._copy_model_type` and `typing.Generic.result_type` are no known symbols); on a union, a
class whose bases are all project classes and builtin types without the member adds no target,
any other one leaves the call unresolved. `object.__new__(C)`, also through an alias, and
`C.__new__(C)` or `super().__new__(cls)` where no class of the MRO defines `__new__` and no base
outside the project but builtin types may, allocate an instance of `C` without running its
`__init__`: a construction of `C`.

Layer 2 `return_type` entries type the results of factories (`sqlite3.connect()` →
`sqlite3.Connection`, `asyncio.get_running_loop()` → `asyncio.AbstractEventLoop`); `"Self"` types
a builder's result as its receiver (`client.with_options(...)`, an instance of a project subclass
included). A call that cannot be resolved is not followed (`unknown_calls`); an external function
that Layer 2 does not list has no effect.

Reading an attribute on an instance whose MRO declares no type for it takes the class an
`[[attributes]]` row of Layer 2 gives (`client.chat` of an `openai.OpenAI` → `Chat`, whose
`completions` is `Completions`, whose `create` the catalog lists), only where the row is
established (`ProjectDecls::catalog_attribute`): the value is an instance (a class object, a
union, `Any`, and the result of a factory the catalog does not know are not typed), the class it
may be under R2 (the class, every project subclass, at most 64 classes, decided once per class
and name) each finds the same row first along its MRO,
no project class on the way binds the name (in its body, on `self`, or on the class object: an
untyped override is no absent one), none defines `__getattribute__` or has a metaclass heki does
not know, and no base heki knows nothing of, or a library class without the row, comes first.
Anywhere else the read is untyped and neither tier reports what follows: `Ty` carries no
provenance, so there is no heuristic type. A store of the attribute (`client.chat = fake`) is
covered by R1, as a replaced library method is.

### 2.2 One `EffectSummary`, propagated bottom-up over Tarjan SCCs
A worklist fixpoint in reverse topological order of the call graph's Tarjan SCCs computes
**execution effects (family 2)**, **mutation and state effects (`MUT1xx`)**, and **write and read
ownership facts (`MUT2xx`)** in one summary. A component's members are visited callees first
(Tarjan pop order); a component exceeding 16 recomputations per member is stopped and its
members are marked `unknown_calls`.

```text
EffectSummary(f) = {
    // Family 2: execution effects
    direct_exec_effects, transitive_exec_effects: Set<ExecEffect>,
    exec_chains:              Map<ExecEffect, ShortestCallChain>,

    // Family 1: mutation and state effects (MUT1xx / MUT2xx)
    mutates_params:           Bitset,                      // parameter i or its elements / attributes changed
    mutated_param_paths:      Map<ParamIdx, Set<AttrPath>>,// attributes / containers changed through parameter i;
                                                           // "[]": an object it holds as an item or an element
                                                           // (p["k"].append(x), for x in p: x.clear()), also
                                                           // through a shallow copy of it (ELEMENTS_PATH)
    changed_param_attrs:      Map<ParamIdx, Set<AttrName>>,// attributes (and "[]") whose objects change (in place
                                                           // or deeper), not just rebound: what a shallow copy
                                                           // shares (MUT202)
    param_change_sites:       Map<ParamIdx, Set<Span>>,    // own changes of parameter i only, never propagated
                                                           // (EFF201 mutates-params counts them)
    escapes_params:           Bitset,                      // stored in an attribute / container / global / closure variable
    reallocates_params:       Bitset,                      // parameter i comes back as a quadratic reallocation
                                                           // of it (DATA101 through helpers)
    consumes_params:          Bitset,                      // iterated to exhaustion on every normal path (MUT106)
    unconditional_forwards:   Bitset,                      // passed on at calls that run on every normal path
    iterates_params:          Bitset,                      // stepped by an iteration on some path: a generator
                                                           // passed there runs there
    reads_self_attrs:         Set<AttrKeyPath>,            // self.<attr> and self.<dict>[const_key] reads
    kept:                     Option<Box<KeptFacts>>,      // only with MUT101 or MUT111 selected
                                                           // (OptInFacts::kept_values):
                                                           // must_read_self_attrs, those read on every path
                                                           // that returns normally, as a whole or with the key;
                                                           // removes_kept, the values kept in an instance
                                                           // __dict__ it may remove or replace on any object
                                                           // (by name, or any); and listed_cache, its decorator
                                                           // is one of the catalog's [[cached_properties]]
    returns_self_attrs:       Set<AttrName>,               // @cached_property: self.<attr> objects the result
                                                           // holds by reference, read for nothing else (MUT101)
    lazily_read_self_attrs:   Set<AttrName>,               // self.<attr> read only where it or a self call
                                                           // initializes it lazily (MUT101)
    mutates_self_attrs:       Set<AttrKeyPath>,            // reassignment or in-place change of self.<attr> (indirect too)
    invalidates_cached:       Set<AttrName>,               // del self.<prop>, self.__dict__.pop("<prop>", ...), clear
    typed_field_write_sites:  Set<TypedFieldWriteSite>,    // own and IndirectHelper writes to (class, attr) with
                                                           // kind, chain, and the parameters the written object
                                                           // may be, plus the sites private callees hand over (a
                                                           // public callee reports its own) (MUT201, EFF201);
                                                           // an item write keyed by a parameter (`EntryKey`)
                                                           // is copied to every caller with the key the call
                                                           // passes (`passed`: a constant, the caller's
                                                           // parameter, or a key heki cannot tell) for MUT201
                                                           // and `heki inspect`; EFF201 resolves the key along
                                                           // the calls its search follows, route by route
    typed_field_writes:       Set<(Class, Attr)>,          // every typed write reached through any callee
    typed_params:             Bitset,                      // parameters the function types itself (its sites
                                                           // cover their changes)
    typed_field_read_sites:   Set<TypedFieldReadSite>,     // own reads of (class, attr): attribute, getattr,
                                                           // __dict__, through an instance or a class
                                                           // object; never propagated (MUT203)
    sentinel_tests:           Option<Set<SentinelTest>>,   // own `obj.attr is S` / `is not` with an
                                                           // `object()` sentinel, per class obj may be;
                                                           // never propagated (MUT118)
    deep_copies:              Option<Set<DeepCopySite>>,   // own `copy.deepcopy(x)` (one argument, no
                                                           // memo) of a project-class object, with its
                                                           // classes; never propagated (MUT118)
    returns_protected:        Option<ProtectedReturn>,     // may return a protected value: producer, depth,
                                                           // call chain (MUT202)
    must_invalidate:          Bitset,                      // invalidation contracts announced on every
                                                           // normal path (MUT109)
    unmet_invalidation:       Set<UnmetWrite>,             // target writes, own or passed up by private
                                                           // helpers, some normal path leaves unannounced
    retains_params:           Bitset,                      // parameters the function keeps (stores, returns)
    returns_new:              bool,                        // returns an object it constructs, nothing else
                                                           // holding it (fresh-objects)
    return_origins:           Option<Box<ReturnOrigins>>,  // MUT112, absent for most functions: what the
                                                           // returned object may be, path by path (new, held
                                                           // state, parameter i, or an element of parameter i;
                                                           // propagated), the project calls whose results (or
                                                           // elements of them) it may return, by span, and the
                                                           // phis its origin walks met (local)
    unresolved_write_sites:   Set<UnresolvedWrite>,        // target-named writes on objects of unknown class,
                                                           // computed names on guarded classes and on
                                                           // objects of unknown class (MUT205)
    unresolved_access_sites:  Set<UnresolvedAccess>,       // reads and method calls named like a strict
                                                           // contract's target on objects of unknown class;
                                                           // never propagated (MUT206, MUT207)
    shared_state:             Option<SharedStateFacts>,    // None for a function that changes no shared state
                                                           // (only with OptInFacts::shared_state: heki inspect
                                                           // and the language server, never heki check):
      changed:                [(SharedState, Span)],       // module globals and enclosing functions' closure
                                                           // cells the own body rebinds, deletes, or changes
                                                           // in place (from every root a merged value may
                                                           // have), first change of each (local); the
                                                           // module body's bindings define its names and
                                                           // do not count, its in-place changes do
      handed:                 [HandedState],               // shared state its calls hand to a project callee
                                                           // as an argument or the receiver (local)
      globals:                bool,                        // a module global changed by the body, a body
                                                           // running in it, or a callee changing what it is
                                                           // handed (propagated)
      cells:                  [CellChange],                // closure cells changed so, except the own
                                                           // variables whose change stays inside the call:
                                                           // rebound by a nested function, or bound only
                                                           // to new containers (new_cells) (propagated)
    capacity_waits:           Vec<CapacityWait>,           // own waits on queues whose capacity the callers
                                                           // of other functions decide (local)

    // Guards: when an effect runs
    exec_guards:              Map<ExecEffect, EffectGuards>, // input tests every occurrence needs; lazy
                                                           // initialization (once per object, from which
                                                           // parameters)

    // Offloading and the sync-driven context
    offloads_blocking_params: Bitset,                      // wrapper running the callable in parameter i on a thread
    sync_driven_only:         bool,                        // internal async def driven only by a sync driver
    unknown_calls:            bool,                        // an own call heki cannot resolve, more than 8
                                                           // CHA candidates, or an A1 hazard (eval, exec,
                                                           // locals(), frame introspection, unmodeled)

    // What iterating the returned object runs (a plain function returning a generator)
    iterated:                 Option<EffectSummary>,       // in the function's own parameters

    // Locks (EFF109; propagated; only with EFF109 selected, OptInFacts::locks)
    locks:                    Option<LockFacts>,           // acquires: non-reentrant locks taken on the
                                                           // function's thread or task (ObjRef =
                                                           // parameter i or its attribute, a global, a
                                                           // closure variable; chain, input guards,
                                                           // tests of state on the way); rebinds: the
                                                           // globals and closure variables it may bind
    exclusive:                Option<Map<ObjRef, ExclusiveUse>>, // objects used one operation at a
                                                           // time on the function's task (EFF110;
                                                           // propagated, never through a call made
                                                           // while a lock is held,
                                                           // CallSite::under_lock; only with EFF110
                                                           // selected, OptInFacts::exclusive)

    // Set orders (EFF112; only with EFF112 selected, OptInFacts::set_order)
    set_order:                Option<SetOrderFacts>,       // flows: the ends (return, an output call,
                                                           // a project call's argument) each source
                                                           // reaches (a take of a set's order, a
                                                           // parameter, a project call's result),
                                                           // local; returns / returns_params /
                                                           // writes_params, propagated
    // Call cycles (EFF114; propagated; only with EFF114 selected, OptInFacts::call_cycles)
    call_cycle:               Option<CallCycle>,           // none, Reaches (a function its body runs
                                                           // is on a call cycle), or On (itself)

    // Metadata
    is_async, is_generator, is_cached_property, is_lru_cache, owner_class_qualname, decorators,
}
```

`is_cached_property`, `is_lru_cache`, and `FunctionDecl::is_property` know a decorator by what
its head is bound to at module scope (`ModuleDecls::decorator_is`): an import by the imported
symbol's last name (aliases included), a function or class of the module itself never, an
unbound name by its spelling.

String constants (`ModuleDecls::str_constants`) name attributes and keys for every rule:
the module's string constants (`heki_ir::ModuleConstants::strs`, whose texts they share), and
module-level names bound once to a constant of another module, each rebound by no other module
unless declared `Final` (`StrConstant::rebindable`, checked against
`ProjectDecls::rebound_elsewhere` where a constant is read), and class-body names declared `ClassVar` or `Final` and bound once, stored on the class or
an instance by no other code of their module (and, without `Final`, by no other module), read
through the MRO when the MRO of every subclass finds the same definition (none defines the name
again or lists another base defining it first); a class that code stores the name on
(`ClassDecl::rebound_attrs` in its module, `ProjectDecls::rebound_elsewhere` from another)
defines it too, and on an instance of a protocol every structural implementation must hold the
same constant (`ProjectDecls::class_str_constant`). A `Final` name without `ClassVar`
(`StrConstant::declares_field`) in a class whose annotations declare instance fields (a
`ClassDecl::dataclass`, or a class a `dataclass_transform` builds) is a field: no constant on an
instance, and on a class object only where each class from it to the field's class keeps the
default as its attribute (`Dataclass::Slots` and transform builds replace it). Each answer is
kept until the indexes change; a recording that reads one again adds what computing it read.
A module's import holds the constant of the imported module (a string, a number or boolean of
`ModuleDecls::literal_constants`, a small display of `ModuleDecls::small_globals`) only where the
module binds the name nowhere else (`ModuleDecls::fixed_import`): `ModuleDecls::rebound_imports`
lists the imports it binds again (another module-level store, a store or `del` in a function
declaring the name `global`), and all of them when a star import, `exec`, or `globals()` may
rebind its names. `TypeCtx::global_literal` and `TypeCtx::global_str` read through it, and so do
re-exports (`ProjectDecls::canonical_value`, which `literal_constant`, `str_constant`, and
`small_global` follow). Contract-only facts
(container entries and `WriteKind::ItemMut`, construction through `ArgOrigin::new_object` and
`TypedFieldWriteSite::construction`, `unresolved_write_sites`) are computed only when
`Contracts::ownership()` is true, that is when write-owners, read-owners, effect-boundaries, or
invalidation contracts are declared. `TypedFieldWriteSite::key` names the entry a write goes to.
`unresolved_access_sites` hold only the names a `read-owners` or `call-owners` contract with
`unresolved = "report"` ends a target with (`Contracts::unresolved_read`,
`Contracts::unresolved_call`), so a summary keeps no read or call no rule asks for. A class the
summaries hand to the contracts (`ContractClass`, in `ArgTargets` too) carries its qualname inside
its module, read from the declarations, only when an `invalidation` target's class or a
`readonly-returns` producer has a module part (`Contracts::names_class_modules`): such a pattern
compares the module and the qualname, which the fqn alone does not tell apart.

`FileSummary.reference_sites` holds, only when a call contract sets `references = "calls"`
(`Contracts::references`), the references handing a project function, method, or class on to
be called later: a load of one (a name, a module or class attribute, a bound method, a closure
cell, which holds what the enclosing function binds to the name, such as a local import; for a
name an import or the module body binds, which the scope binding it reads without a load of its
own, each use handing it on) whose value
reaches, through copies, phis, and the displays holding it, an argument of a call, a stored
attribute, item, or global, a `return`, a `yield`, or a `raise`. Each is a `CallSite` of what
calling the value calls (the class it constructs, or its project callees, without bindings),
at the load, and none is in `call_sites`: no call edge, effect, or binding comes from them.
Uses that only name the value hand nothing on: the callee of a call (a call site of its own), a
callable a library API runs at the call (`CallSite::handed_callable`, also through the
`functools.partial` object it is), an argument of a builtin that only inspects its arguments,
of `super`, of the typing forms `cast`, `TypeVar`, and `NewType`, and of `contextlib.suppress`; a
receiver, an operand, a subscript, a dict key, a closure's captures (defaults and eagerly
evaluated annotations are not told apart, nor a class's bases and keywords), and a module
binding a name to the value itself (an import, a module alias). With `references =
"calls-and-bases"` (`Contracts::reference_bases`), the bases of a class statement are reference
sites too (`CallSite::is_base`): one per value the statement passes as a base
(`ModuleIr::class_base_values`) that is a project class, typed in the statement's block as any value
is (a parameter or a local of the imported class's name is another value), constructing that
class, at the base's load when the statement loads it, else at the class statement. With
`references = "reads"` (`Contracts::reference_reads`, which includes the bases), a load reaching
no use handing it on but another use is a reference site too (`CallSite::is_read`): the object of
an attribute access or a method call, an argument of a call that only names it, a subscript, an
operand, a tested condition, an iterated, awaited, formatted, or `with` value, a dict display's
key, a `**` unpacking, and the statement uses (`ModuleIr::statement_uses`), through copies,
phis, and the displays holding it as for a hand-on; a callee, a closure's captures, a binding of
another name (a module name, a `nonlocal` cell), a deletion, and a use inside an annotation the
function evaluates (`ModuleIr::annotations`) are none. For a name an import
or the module body binds, a read is at each use that is not one handing it on. The values
reaching each kind of use are found with `heki_ir::walk::reach` under one budget per function
(`summary::reference_sites`); past it the function has no reference site. The class
bodies a module runs add their references to the module body's (their other facts are not the
module's). Like `call_sites`, they are local facts: the language server compares a function's references with
its call sites (`Workspace::caller_view`) and shows them in call chains.

`CallSite.arguments` holds, only when a call target has an argument list
(`Contracts::marked_params`), the constants the call passes for the parameters those lists name,
those every callee the call may run, on every way the mappings it unpacks may be built, agrees
on (`summary::keywords::agreed_marks`): by keyword, by position, or as a default decided by its
class and truthiness, as the parameter binding of the extraction binds them, a keyword only for a
class without a project `__init__` and for a library method building its receiver's class
(Layer 2 `builds_receiver`: the keywords of `model_construct`, the entries of the mapping
`model_validate` and `model_copy` take, whose site is a construction of the receiver's class,
`CallSite::constructs`). That binding (`BoundCallee::params`, and the rules'
`bind_arguments`) follows Python's: a position binds the parameter of its slot only among the
positional parameters (`FunctionDecl::positional_param`; one past them lands in `*args`, a new
tuple, and binds no parameter, never a keyword-only one: only a hand-over asks whether the callee
keeps `*args`, which holds it), and a keyword never binds a positional-only parameter
(`FunctionDecl::keyword_param`; one binding no parameter is, for the argument lists, a parameter of
its own name in `**kwargs`); the receiver is the first position. A `**` operand that is a
mapping the function builds passes its entries as keywords when nothing changes it, on each way
it may be built (`Extractor::mapping`): a dict display with string constant keys, a merge of
mappings (a display's `**` entries, `dict(...)` of at most one mapping and keywords, `a | b` of
two mappings the function builds), whose later entries replace earlier ones and where a mapping
heki does not build replaces every key before it, and a phi of mappings, one way per way of each
input, at most `MAPPING_WAYS` (8) ways, past which it may pass anything. Each value holding a
mapping, through copies and phis, is only copied, merged into another mapping, unpacked with `**`,
read as the field values of a `builds_receiver` call, tested, returned, or raised
(`Extractor::only_unpacked`). The mappings a mapping is built from are found with one `reach`, and
their ways with a `fixpoint` over them (a mapping a loop merges into itself grows each pass); the
answer of each mapping decided is kept, so a later walk stops at it, and the entries a call binds
are taken from the function's budget, past which the mapping may pass any keyword. Where a call
unpacks several ways, the parameter binding of the extraction takes the keywords written and any
other; any other `**` operand may bind any parameter. A call of a `functools.partial` object the
function makes through copies (`Extractor::partial`) is a call of the callable it holds, passed the
positions it holds first and its keywords, which a keyword the call writes replaces
(`Partial::called_with`); one unpacking a mapping over keywords the object holds stays
unresolved. A callee the call raises `TypeError` for instead of running
(`Bindings::raises`: two keywords of one name, a parameter bound twice, a keyword without
`**kwargs`, a position past the parameters without `*args`, a parameter without a default left
unbound where no unpacking may bind it, counted from `FunctionDecl::required_positional` and
`required_keyword_only` in the work of the call's arguments) is passed nothing. The constants are
literals, string constants (`TypeCtx::str_constant_with`), and modules' number and boolean constants
(`TypeCtx::module_constant`: a module-level name, an import, or a module's attribute,
`TypeCtx::global_read`, the reads catalog cases follow too). The marked names are looked up for the
parameters a call binds, so the work per call is that of binding it, whatever the number of argument
lists.

`Dispatch.recv_params` holds, only when a `call-owners` contract allows the calls through a
method's own receiver (`Contracts::own_receiver_calls`), the parameters of the caller the receiver
of a class method's or a static method's call may be, as itself (`super()` is the method's own
`self`): the first bound parameter of the callees (`BoundCallee::params`) does not tell it, since
a class method binds the class and no value of the caller, and a static method no receiver at
all. It sits behind the call site's boxed dispatch, which few sites have, so call sites keep their
size.

`TypedFieldWriteSite.through_protocol` marks, only when a `write-owners` contract allows writes
through protocols (`Contracts::write_protocols`), every site of a write whose object is typed with
a project protocol declaring the attribute itself or through its protocol bases
(`ProjectDecls::protocol_declares`): the protocol's own site (its class is the protocol) and each
implementation's, which keep it as they are handed up through private helpers. The rules find the
protocol of a write among its sites (`mut_rules::protocols_written_through`, once per function);
a flag fits the room the site's layout leaves, where the protocol's name would grow every site. It
reads the protocol's declaration, which may sit in another module, so it takes part in Early
Cutoff.

`CallSite.returned` (whether the call's result may be returned, and how) is computed at
extraction for every project call. `CallSite.flow` (the invalidation contracts announced after
the call on every normal path, and the next call; set on `Iterate` / `Through` sites too), with
`FileSummary.flows`, is computed from the CFG and its post-dominator tree, only when a contract is
configured. `ProjectIndex` keeps no IR.

`FileSummary.cache_flows` (`heki_effect::CacheFlow`, one per definition) holds the `MUT101` facts
of every method that writes a `self` attribute in its own body: the writes (attribute, position,
whether they rebind the attribute, fill a memo, or initialize it lazily), and the blocks of the method (normal
successors, returns, exception targets) with the points that invalidate or read a cached
property of its own instance (`del self.<name>`, a constant-key removal from `self.__dict__`,
`self.__dict__.clear()`, `self.<name>`, and calls on `self`, whose callees' summaries say what
they do). They are local facts, never propagated, and they let `MUT101` decide staleness from the
index alone, for a method of any indexed file. `FileSummary.kept_removals` (only with
`OptInFacts::kept_values`) holds the names under which the file's functions remove a value an
object keeps in its `__dict__`: a project-wide fact the language server compares per file
(`Workspace::note_kept_removals`), as it does the sentinel comparisons of `MUT118`.

`OptInTables::blind_spots` (`heki_effect::BlindCall`, in `FileSummary::opt_in_tables`, the one
box per file that holds the opt-in tables of its functions; only with `OptInFacts::blind_spots`,
which `heki_rules::recommend_facts` sets and neither `heki check` nor the language server does)
holds, per function, the calls heki is blind to in its own body and the class bodies it runs,
each once in source order: a call resolving to no function (`Callee::Unresolved` or `TooMany`),
with the name called and why, from the receiver or the called value (`UnresolvedReason`: an
unannotated parameter, an annotation giving no class, `Any` or `object`, a `Callable`, a name nothing heki
sees binds, an unlisted protocol, a call result, an attribute, a computed `getattr` name, more
implementations than CHA follows, a member heki does not find, any other untyped value). A call
on a value of `Any`, `object`, or `Callable` (an instance or a class object), or of an unbound
name, resolves to no function: its library name (`typing.Any.get`, `builtins.create`) is no
library's, so the call is unresolved for every consumer (`EffectSummary::unknown_calls`, the
purity `heki inspect` gives, blind spots); and a call of a
library function or class outside the project and the standard library without a catalog entry
(`Blind::Uncatalogued`, with whether the catalog lists its package). They are local facts, never
propagated, and no rule reads them. `ProjectIndex::blind_spot_reach` walks a function's facts of
one kind, its body or what iterating its result runs, as each call hands them on
(`ProjectIndex::delivery`: a helper returning a generator it does not iterate runs no generator
body where it is called), once from every `async def` (not through a worker thread) and once from
the calls inside a loop or once per item, giving the functions on an event loop and below a loop; `heki_rules::blind_spots` counts the calls by package and name and by function and reason,
and the hidden `heki dump-recommend-facts` prints them. `heki recommend` lists them
(`heki_rules::recommend::blind_spots`), after two trials that build no fact of their own: a run
of `EFF101`, `EFF111`, `EFF103`, and `EFF113` with a hypothetical catalog entry
(`blocking_net`, `db_query`) for each of the 64 library names with the most reach, whose
reports count for the name at the end of their chain; and the module names of the files with a
candidate directory added to `src`, by which it resolves the imports again.

The census contract candidates are drawn from (`heki_rules::census`) is a pass over the index and
computes no fact of its own: every local summary keeps the function's own typed read and write
sites for every project class, whatever the contracts. Per class (the one heki types the accessed
object with) and attribute it lists the functions writing it outside the initialization of their
own instance (whether they rebind it, change its object in place, or only fill an object they
construct), the initializers writing it on their own `self` (`INITIALIZERS` of the class), and
the functions reading it (a read through a protocol declaring the attribute is the protocol's);
per class, the modules constructing it or accessing its attributes; per module, its functions,
those whose final summary reaches a blocking effect, and those reaching code heki does not see
(`ProjectIndex::reaching_blind_spots`: a blind call, `unknown_calls`, or a body known only from a
stub, in their own body or in what their calls hand on, as `blind_spot_reach` walks it, one
`Closure` over those states). Without an ownership contract the summaries keep no write into an entry of an attribute's
container and do not answer whether a write constructs its object, so a contract trial sees more.

A report of a contract rule records the contracts it is of (`Diagnostic::contracts`: their places
in the configuration's array of the rule's kind; written by no output format). `heki recommend`
declares its candidates together in one trial and counts each report for each contract it is of.
One access several contracts report is one report standing for all of them: the strict rules
(`MUT205`, `MUT206`, `MUT207`) key their reports by the name accessed, and `check_project` keeps
one report per access, rule, and target. `MUT202` records none: the summaries keep its producers
without their contract.

**Guards.** An effect inside a branch of a test of the function's own inputs (`if cache:`,
`if x is None:`, `isinstance(x, T)`, `hasattr(x, "write")`, also through `and` / `or` / `not`,
early returns, and conditional expressions; on a parameter, or an attribute of one the function
never assigns) records that test. A test of a value a phi merges (`if buf is None: buf =
StringIO()`, then `if isinstance(buf, str): open(buf)`) is read along each edge into the phi:
the value the edge brings decides the test, or is an input whose test it becomes, and what holds
on that edge holds with it (facts are never killed); an edge whose value decides against the
outcome drops out, so `open` above needs `buf is not None` and `isinstance(buf, str)`. An effect
inside a lazy initialization (`MUT107`'s branch, when every way out of it sets the attribute)
runs once per object, as does every effect of a `functools.cached_property` getter, through any
callee (its result is kept in the instance's `__dict__`); one under the same test of a module
global the function assigns (`global _client`) runs once per process, unless the function also
assigns it a constant the test finds unset on a way to a normal return or back to the test
(`_client = None`; one before a `raise` only undoes a failed initialization). At each call the
callee's guards are mapped to the caller (`through_call`, wherever a callee's facts reach a
caller): a constant argument, a value whose exact class the caller knows (`ConstArg`: a display
or comprehension, an `io.StringIO` / `io.BytesIO` it constructs, or a phi of values of one such
class), the literal default of an omitted parameter, or a test the caller has made on the same
value that decides a guard against the effect prunes it there; an undecided test of a caller
input becomes the caller's guard. A known class decides `is None`; `isinstance`, true when a
tested class is in its MRO and false when every tested class is a builtin type, one of the `io`
classes, or `os.PathLike` outside it; and `hasattr` for the attributes of the file protocol
(`read`, `write`, `close`, ...), which the buffers have and the builtin values lack (a buffer may
gain attributes of its own, so a name outside the protocol, or one a buffer lacks, stays
undecided). A generator's guards are decided at the call that creates it. `EFF201` does not read
guards. `EFF102` /
`EFF103` do not count a lazily initialized effect per iteration unless a data-sized loop around
the call constructs the object. Within one function, a call also runs at most once per run of a
loop (`CallSite::once_loops`) when every way on from it leaves the loop (`break`, `return`,
`raise`; a handler that goes on with the loop is a way back), and when a test of a local
variable the loop carries lets it run only until the variable changes: every path from the
branch back to the test assigns the variable, and every value it can have there after an
iteration takes the test's other branch (`if conn is None: conn = connect()`; a first-iteration
flag `if first: ...; first = False`; a constant decides by its value, and another value counts
as set only when the branch computes it, as for an attribute). The regions and the walks that
find them take at most 8 cells per byte of the function's source (at most 250,000); past that
none of the function's lazy initializations is recognized. A blocking wait on a future known to be done returns at once: the catalog marks the
waits (`waits_until_done`: `concurrent.futures.Future.result` / `exception`) and the calls
whose awaited result finishes a future (`done_when_awaited`: `asyncio.wrap_future`), and a
forward must-analysis with the input tests' (one atom per future, which only ever holds) knows
a future done after `if f.done():` and after such an `await`. Anything not recognized keeps the
effect.

**Queue capacities** (`capacity`). A `put` waits only while its queue is full (`waits_while_full`:
`queue.Queue.put`, `LifoQueue.put`, `PriorityQueue.put`), which only a queue its `maxsize` bounds
can be (`bounded_by` on the classes: a positive integer bounds it; none, zero, a negative one, or
`None` does not). Its blocking effects run only on a queue known to be bounded: `Capacity` is a
`heki_ir::walk::Answer<Bound>`: `Open` when some path is not known, otherwise
`Known(Bound::Unbounded)` or `Known(Bound::Bounded)` (some path bounded). The join is open when
either side is, otherwise bounded when either is, and `None` (no queue) adds nothing. What a value
gives a queue (`CapacityOf`; for an integer, the capacity of a queue built with it as `maxsize`) is
the join over what it may come from through copies, phis, and a construction's `maxsize`, every
value of a function walked once (Tarjan; a cycle's members share one answer): a construction, an
integer constant (`FnIndex::const_int`), an attribute of `self` or of a typed project instance, a
module global (also read as a module's attribute, or bound by an import statement, also one inside a
function), a parameter, anything else unknown. The declarations record what a module's class bodies
and methods store in `self` attributes (`ClassDecl::attr_capacities`) and what it binds its globals
to (`ModuleDecls::global_capacities`), for the names one store of which is a queue (or, for a name a
`maxsize` reads, an integer), naming the attributes, globals, and parameters they take the rest from
(a module that neither imports a catalogued queue class or its module, at any scope, nor types an
attribute or a global as one records nothing); `ProjectDecls::attr_capacity` resolves an attribute
over every class whose stores an instance may hold (the class, its subclasses, and their MROs; a
class defining the name without a record, or a name another module stores, is unknown) and
`ProjectDecls::global_capacity` a global over its module (unknown without a record or when another
module rebinds it), following what the records name (each attribute and global resolved once while
the indexes stay, the names on one cycle sharing one answer), so a summary's capacities name
parameters only. A wait the code decides is an effect like any other. A wait on the function's own
parameter is an effect needing a bounded queue there (`EffectGuards::bounded_queue`): each call
decides it by what it binds (a queue not known to be bounded prunes the effect there; the caller's
own parameter carries the need on), so a helper waits only for the callers handing it a bounded
queue. Any other wait is a `CapacityWait` of the summary, and every such call keeps it in
`CallSite::queues` (`SiteQueues::while_full`). Before propagation, the index resolves the parameters
these name, and those queue arguments name (`SiteQueues::handed`, every parameter a call's binding
can name, so one pass over the calls finds every source), over the call sites binding them
(`ParamCapacities`, a join fixpoint over a worklist of the parameters each one's value depends on, a
value growing three times at most): the argument's capacity as its caller found it, or its constant
(`ConstArg::positive`, also of a literal default); an argument not bound exactly, and a function
without project callers, are unknown. Propagation adds the waits the parameters show bounded to the
summary's effects (`ParamCapacities::realize`); a site's `while_full` effects run where the
parameters show its queue bounded (`ProjectIndex::while_full_effects`, which Boundary-Only Reporting
and `EFF201` read), and an `async def` reaching a wait that needs its own parameter bounded reports
it only when its callers' join is bounded. An incremental update recomputes the parameters'
capacities; the functions whose waits or calls name a parameter whose capacity changed are
re-propagated and reported changed, also when their summary stays equal (the report at their own
call moves).

**Where a body runs.** A call site hands its caller one of a callee's two fact sets (one home,
`ProjectIndex::delivery`): its *body* (`Body`), or what iterating the object it returns runs
(`Result`: a generator function's body, or a plain function's `iterated`). A call (`SiteRole::Call`)
runs the body of a plain function, of an `async def` only when it is awaited or driven (calling one
only creates a coroutine), and never of a generator function (calling one only creates the
generator; `FunctionDecl::is_generator`: no decorator other than `staticmethod` / `classmethod` /
`abstractmethod` / `override`). The object a project call returns gets one more site, with the
call's callees and bindings, wherever the calling function uses it, found by the shared iteration
table (`consumption`):

- `Iterate`: it is iterated there (a `for` loop, a comprehension: inlined from Python 3.12, a frame
  of its own receiving it before; unpacking; `yield from`; a consuming call such as `list(...)` or
  `items.extend(...)`; a lazy wrapper iterated there, such as `enumerate(...)` or `zip(...)`): its
  `Result` runs in the caller's body;
- `Return`: a plain function returns it, directly or through lazy wrappers: its `Result` goes into
  the function's own `iterated`;
- `Through`: it is passed to a project call whose body iterates that parameter (`iterates_params`)
  at the call, or whose result is iterated there or returned by a plain caller (`return
  relay(rows())`): its `Result` runs inside that call, or goes into the caller's `iterated`.

Each of these sites records the span of the call that made the object (`CallSite::made_by`), and
so does the site of the `await` of a coroutine made earlier: what one call runs is read from its own
site and those (`CallSite::of_call`, the call's span and target), never from another call of the
same callees with the same arguments, whose target is the same.

Any other use runs nothing in the caller: a generator stored, yielded, returned from an
`async def`, handed to an external or unresolved call (`StreamingResponse(rows())`), or only
stepped with `next()` (which runs it only to its first `yield`, which the summary does not separate
from the rest of the body). Generator expressions keep running where they are written, where they
are almost always consumed. Every fact moves with the body: execution effects and their chains,
parameter mutations, escapes, and slots, `self` attribute reads and writes, typed write sites, and
the read and call sites the contract rules walk to (`MUT203`, `MUT204`, and `EFF201`'s
`reads-targets` / `calls`: only the call itself is a call, never another site of its result); a
chain is the call stack where the body runs (the
generator is called from the iterating loop, not from the function that returned it, and from
inside a function iterating it). The contract rules walk the same deliveries backwards over
`(function, Body | Result)` states, so a private helper hands its reports only to the callers that
run it: a private coroutine that is only scheduled (`asyncio.create_task`) reports its own writes.

A function decorated with a catalog entry offloading argument 0, applied directly or as a factory
(`@sync_to_async`, `@sync_to_async(thread_sensitive=False)`), is `d(f)`: calling it is a call of
`d` with the function as its offloaded argument (`FunctionDecl::offloaded_by`, resolved through
imports and re-exports when the declarations are completed; every definition of the name must be
decorated so), and its body runs at the call on a worker thread: a site of its own, whose
`CallSite::runner` is `Runner::Worker`. The callable an offloading API is given runs so too, as a call of the
project function with the positional arguments after it (`asyncio.to_thread(f, a)` calls `f(a)`,
`loop.run_in_executor(None, f, a)` too; keyword arguments are left unbound, since some APIs take
their own). Every fact of such a site moves to the caller except the blocking effects, which do
not block the caller's thread: its queries per item (`EFF103`), its clients (`EFF102`), and its
changes to shared objects (`MUT202`, which reads the offloading call the same way) count, and
`EFF201` does not enter it. A callable a Layer 2 `sync_facade_arg` API runs
(`AsyncSession.run_sync(fn)`) gets a site with `Runner::SyncFacade`: it runs on the caller's
thread, so only its `blocking_db` (the database I/O through the synchronous facade it receives,
awaited for it) is dropped; its arguments are left unbound. The callable a builtin calls once
per item (`consumption::per_item_callable`: the function handed to `map`, `filter`,
`itertools.starmap`, `itertools.filterfalse`, and `functools.reduce`, and the `key` of `sorted`,
`list.sort`, `min`, `max`, and `itertools.groupby`) gets a site with `Runner::PerItem` at the
builtin's call: every fact of its body runs there, on the caller's thread (the lazy builtins run
it where their result is consumed, almost always where they are written, as for a generator
expression). Its parameters after the arguments a partial holds take the items, which are no
value of the caller, and the ones after the items get their literal defaults when the builtin
fixes how many it passes (one item, one per iterable of `map`, or the accumulated value and an
item for `reduce`; not the elements of a `starmap` item); what the builtin returns is never what
the callable returns.
`CallSite::per_item` names the builtin and the arguments it takes the items from, and `EFF102` /
`EFF103` count the site as inside one more loop, as long as the items, which they size as a
`for` loop's iterable (`Sizes::items_data_sized`); the receiver, the arguments a partial holds,
and the items stay the same objects through that loop (`ArgBinding::stays_in_loops`), so a lazy
initialization on them runs once per object. A class handed to such a builtin is constructed per
item, which is not modeled, and a lambda the function makes next to another lambda is not
followed: every lambda of a function is `<function>.<locals>.<lambda>`, one key whose summary
holds the facts of all of them. A builtin calling a
callable per item and a sync-facade API resolve a `functools.partial` object to the function it
calls, the arguments it holds binding the leading parameters (`partial(f, a)` handed to `map`
calls `f(a, item)`); an offloading API's partial is not followed, since `MUT202` matches a
worker's site to the arguments after the callable again (`site_call`).

Early Cutoff compares summaries without their source positions, so an edit that only moves code
changes no caller's facts. The positions callers copy (call chains, write sites) are still
re-propagated into their summaries, so that the incremental index always equals a full build, and
the language server re-checks the files whose diagnostics show a moved call site. The call chains
of `MUT108` / `MUT110` / `MUT120` reports are no summary facts: the rule walks the own summaries
and project calls of the functions below the call (`FileSummary::local_functions` and `call_sites`) down to
the first function whose body makes the change. A heuristic `MUT110` report, whose chain names no
line, records "Changed at the line" open when the run asks (`CheckExtras::open_premises`): no
function on the chain makes the change in its own body (a generator another function returns is
iterated), or the one that does is not where its own summary establishes a builtin container whose
changing method no project subclass replaces (`EffectSummary::param_inplace_sites`, which keeps no
reason). The language server compares these facts of the
edited file's functions that change a parameter in place (`heki_rules::chain_facts`) and
re-checks the files with such reports above a function whose facts changed. Likewise, while
`MUT111` is selected, it keeps the stale writes `MUT101` finds in each file's methods, with the
classes whose instances run each writer (`heki_rules::stale_writes_by_file`), as of the file's
latest analysis; when they change, the files reading those cached properties are re-checked.
While `MUT118` is selected, it keeps the sentinel tests of each file's functions without their
places (`heki_rules::sentinel_tests_in`); when they change, the files with deep copies are
re-checked, and a test that moves moves the related locations showing it, as a call site does. A
`MUT113` report shows where other methods change a container (`self_container_change_sites`):
the language server re-checks the files showing a change site that moved, as for a call site. A
place it records keeps what is there (a call's callees, a change's attribute), so that two calls
or changes of one length swapping on a line, which keep every span, count as moved too.

The incremental index also records what summarizing each file read of the project declarations
(`FileSummary::decl_reads`: the modules looked up, the classes whose subclasses were listed, the
property getter names tried, the names tried among the functions returning an attribute of a
parameter), and the language server records what each check read. An update of a
module's declarations reports the keys it changed (the module, and the hierarchy indexes that
differ); every file that read one of them is re-analyzed. This covers names resolved through
re-exports, class hierarchy analysis across modules, and property getters, without depending on
which module imports which.

### 2.3 Family 1 (`MUT101` / `MUT103` / `MUT106` / `MUT107` / `MUT109` / `MUT111` / `MUT112` / `MUT113` / `MUT118` / `MUT119` / `MUT201` / `MUT202` / `MUT203` / `MUT204`)

#### `MUT101`: mutation of attributes a `@cached_property` depends on, without invalidation (strict Stage 0-A version)
1. For every `@cached_property` method `p` of a class `C` (with its subclasses; a nested class is
   a class of its own), take `reads_self_attrs(p)`: the `self.<attr>` and constant-key
   `self.<dict>["k"]` reads of `p`, directly and through `self.helper()` calls, without `p`
   itself.
2. For every method `m` of `C` and its subclasses, and every method `C` inherits from a project
   ancestor that no class before it in the MRO overrides, take the writes in `m`'s own body,
   **except**:
   - **initialization and two-phase initialization hooks**: `__init__`, `__post_init__`,
     `model_post_init`, `__setstate__`, `__enter__`, `__new__`, `setup`, `setUp`, `postinit`,
     `_open`, `_setup`, `setup_*`, `_setup_*`, `_init_*`, `_configure_*`, and private helpers that
     an initializer calls;
   - **memo and cache dictionaries**: in-place changes of attributes with a `cache` / `memo` word
     in their name (`_cache`, `_memo`; rebinding them and `memory_*` names count), `setdefault`,
     and lazy filling under `if k not in self._xxx:`;
   - **disjoint constant dict keys**: when the keys `p` reads (`self.params["outtmpl"]`) and the
     keys `m` writes (`self.params["proxy"]`) are all constant and disjoint.
3. Report **`MUT101`** when an attribute in `reads_self_attrs(p) ∩ writes(m)` remains at a write
   that leaves a stale value on some normal path of `m`: neither does every path after the write
   invalidate `p` (`del self.p`, `self.__dict__.pop("p")` / `clear()`, a `self` call whose
   `invalidates_cached` holds it), nor is `p` invalidated on every path to the write with no read
   of it since (a private helper all of whose callers invalidate is covered).
4. For an attribute in **`returns_self_attrs(p)`**, only rebinding writes count (`self.a = ...`,
   `del self.a`): the cached value holds the attribute's object itself and sees in-place changes.
   The fact is local to the getter (every read of `self.a` is returned as itself or put, as
   itself, into a display or a container that is returned; `self` reaches no other code), minus
   the attributes the `self` methods and properties it calls read (final summary). A call whose
   result the getter discards (`CallSite::result_discarded`: no instruction, phi, or terminator
   reads it) takes its reads away only when it writes an attribute reaching the value: one the
   getter reads itself or through a call reaching the value (a fixpoint over the getter's
   discarded calls, `callgraph::recompute`). A starred
   display element (`(*self.a,)`) is a copy: the IR lowers it to a copy named `.unpacked`.
5. For an attribute in **`lazily_read_self_attrs(p)`**, a lazy initialization does not count: a
   write in a block that only the "unset" outcome of a `None` / falsy test of `self.a` leads to
   (`if self.a is None:`, `if not self.a:`; the blocks that branch dominates, normal and
   exception edges). The fact holds when `p` reads `self.a` only in functions that initialize it
   lazily: the methods writing it so (the test and every other read of theirs count as lazy),
   joined through `self` calls and property getters, where any other read of it removes it.
   Computing `p` ran the initialization, so the test cannot find the attribute unset again
   (a write setting it back to `None` is a write of its own).
6. The writes and the paths after and before them come from `FileSummary.cache_flows`, so the
   stale writes of a method are known without its IR: `MUT101` reports those of the checked
   methods, and `MUT111` reads those of every method.
7. **Reads on every path** (`KeptFacts::must_read_self_attrs`, computed only when `MUT101`
   or `MUT111` runs: `OptInFacts::kept_values`): the attributes a function reads on every path
   on which it returns normally, exception edges into handlers included. Its own loads of
   `self.<attr>` are points of the path facts (`Progress::Returned`: a load that raised read
   nothing); the loads of one attribute whose value is read as a whole, and the item loads of
   one constant key of values read only through such loads, are asked together
   (`Paths::on_every_return_passes_one_of`), so a read on each side of a branch counts. A `self` method
   called on every such path, or a property read there (its getters' call), adds what every one
   of its targets reads, when the function's name is bound once (each site's paths are its own
   definition's), the call's targets are every implementation it can run (`CallSite::complete`;
   for a read, the getter every class the object may be finds first:
   `TypeCtx::getter_targets_complete`), and each runs its body at every call (no cache
   decorator); a callee of the function's own call-graph component adds nothing,
   so propagation needs no fixpoint for it. An attribute read as a whole covers each of its
   keys, also where two definitions or two targets are joined.
8. **What may remove a kept value** (computed only with `OptInFacts::kept_values`). A method's
   `CacheFlow` also holds the points that may remove or replace a value kept in some object's
   `__dict__`. Each is decided positively: a point is harmless only where heki establishes it.
   `MayRemove(name)` stands for a store or a deletion of `obj.<name>`, or of an item of a
   `__dict__` under that key, through any reference (`self`, a parameter, a closure cell, a
   global, a container's item, `*args`), when `<name>` is a property getter's name
   (`ProjectDecls::names_property`, recorded as a `DeclKeys::property_names` key) and `obj` is
   not established as an object that keeps no value under it
   (`heki_effect::invokes::may_keep_under`: a value built in the function, or an instance of
   classes none of which binds a property getter of the name, `ProjectDecls::binds_property`).
   `MayRemoveAny` stands for a store or a deletion of a `__dict__` or a `__class__`; a `__dict__`
   cleared or handed to a call; an `await`, the implicit awaits of `async for` and `async with`,
   or a `yield`, while other code runs; a call heki cannot resolve, of a library outside the
   catalog, or of one that may run project code by its `[[invokes]]` entry, section 4.2; a
   project call whose targets are not every implementation it can run; the iteration of a value
   that is no builtin container, no project generator's result, and no `lazy` call's (a
   generator expression runs its body); a library call handed a container whose items heki does
   not know; a special method an instruction runs on an operand's class
   (`heki_ir::IrFunction::special_methods`: an operator and its reflection, with what a builtin
   operator reads of the other operand, a comparison, `in`, `%` formatting, a conversion or
   format, a subscript and its key, an unpacking, a call's `*` and `**` arguments, a key a set or
   dict display hashes, a generator expression's outer iterable, entering and leaving a `with`,
   and a branch's truth test, not a loop's test of its next item) that heki does not establish
   as no code of the project (`heki_effect::invokes::special_methods_run_project_code`: on every
   class the operand may be under R2, the method found first along the MRO,
   `ProjectDecls::special_method_owner`, is `object`'s, a builtin type's, whose hash,
   comparison, and `repr()` run the items' own and whose dict or set lookups compare stored keys
   under an equal hash, or a library class's called as a library call handed every operand,
   R3; a value of an abstract class, a protocol, `object`, or no class heki knows establishes
   none); an attribute access that may run code of the project besides a getter heki resolves
   (`heki_effect::invokes::attribute_access_runs_project_code`: on a project class, a read that
   finds no plain attribute or method, a write that may run a property setter, `__setattr__`,
   `__delattr__`, another descriptor, or code of a base heki does not know,
   `ProjectDecls::attr_write_runs_no_code`; on a library object, the access as a call of that
   attribute under R3; on a value of unknown class, anything); a class statement; an import of
   a project module; a `raise` of a value that may be a class of the project (an instance, a
   value built in the function, and what calling a class gives are none). `Calls(span)` stands
   at the project calls no `SelfCall` stands for (other receivers, the property getters a read
   of `self.<name>` runs) and at each step of iterating what a project generator called at
   `span` returns, whose callees' summaries say what they remove. What the function
   establishes of a value's class is one least fixpoint over its instructions and phis
   (`Extractor::established_values`, `Built`): a constant, a string, a `bool` of `not`, `is`, or
   `in`, and a tuple of such values are builtin values whose items are established; a list,
   set, or dict display and a new container a builtin returns are of an exact builtin class
   with items heki does not establish; an operator, a comparison, a conversion, or `str()`
   gives a builtin value only when every operand is one or its inferred class runs `object`'s
   or a builtin type's own methods for it (`invokes::interpreter_own`), and a comparison of two
   sequences by order only when their items are established too; anything else is not
   established.
   `KeptFacts::removes_kept` (`EffectSummary::kept`) joins the same points of a function (a
   `yield` excepted: its consumer is its caller) with what every callee removes, and
   `FileSummary::kept_removals` the names under which the file's functions themselves remove a
   value (also where the deletion may run other code): the lazy-setup filter asks whether any
   function of the project does, and the language server checks again the files of the lazy
   setup getters of a name whose removals change. Each write of
   the flow also records, under the same opt-in (`SelfAttrWrite::kept`), its literal key
   (`KeptWrite::key`) and, for an in-place change, the builtin list, dict, or set the attribute
   holds by its type and the method that changes it (`KeptWrite::change`).

#### `MUT111`: readers of a stale `@cached_property` (opt-in)
1. Reads: the `TypedFieldReadSite`s of each checked function's own body (`self.p`, `obj.p`,
   `getattr(obj, "p")`, `obj.__dict__["p"]` / `.get("p")`), whose class `K` heki knows; a read
   through a class object gives the property's descriptor, not a cached value, and is none.
2. For `K` and each project subclass `X` of it, the getter an instance of `X` runs for `p` (the
   first definition along its MRO); a `@cached_property` getter is a candidate.
3. Its stale writes are `MUT101`'s (whatever is selected, suppressions included). A write by `m`
   reaches the read when `m` runs on an instance of `X`: `m` is the first definition of its
   name along `X`'s MRO, or every definition before it calls the next one on `self`
   (`super().name()`) and does not invalidate `p` itself.
4. One report per function and property name, at the first read a write reaches; the target is
   `Class.p` of the property, the message names the first write that runs on an instance of `K`
   itself (else the first), and every reaching write is a related location. A getter or helper
   that reads `p` is reported itself; its callers are not.

#### `MUT118`: an `object()` sentinel lost in a deep copy
1. **Sentinels** (`ModuleDecls::object_sentinels`): the module-level names bound once, outside a
   loop, to a call of the builtin `object` with no argument, with the conditions of a string
   constant (no import binds the name; no star import, `exec`, or `globals()` may rebind it).
   `ProjectDecls::object_sentinel` reads one through re-exports and drops it when another
   module rebinds or deletes it (`ProjectDecls::rebound_elsewhere`).
2. **Held** (`ClassDecl::named_attrs`, read by `ProjectDecls::held_sentinels`): what a class's
   own code binds an attribute to among the names that may be sentinels (its module's own, a
   name an import binds, `module.NAME`). The class body's binding (`attr = S`, `attr: T = S`,
   `field(default=S)` in a stdlib dataclass) is the class attribute; the default of a field the
   generated `__init__` of a stdlib dataclass stores, a store `self.attr = S` in a method, and a
   store `self.attr = p` of a parameter whose default is `S` are held by the instances (a `def`
   in a class body hands its function object the defaults first, in parameter order). An
   annotated default under a base the catalog describes as keeping hashable defaults
   (`keeps_hashable_defaults`, R3) is held too, unless something may validate it (the
   annotation names `validate_default`, `NamedAttr::validates_default`, or a project class along
   the MRO binds `model_config` or `Config`); under another base outside the project, or with a
   dataclass's own `__init__`, it may be (open).
3. **Tests** (`EffectSummary::sentinel_tests`): a comparison with `is` / `is not` of an attribute
   load with a name that `object_sentinel` names, recorded per class the object may be, with
   whether that is one class (no union, no protocol). Reading the attribute runs no code
   (`ProjectDecls::plain_sentinel_attr`: as `plain_attr`, where a class body binding the name to
   a sentinel binds no descriptor, and a base whose `__setattr__` the catalog describes as
   `stores_on_instance` reads the stored value).
4. **Deep copies** (`EffectSummary::deep_copies`): a call that resolves to `copy.deepcopy` with
   one positional argument and no keyword, of an object whose project classes heki knows.
   `ProjectDecls::deep_copy_of` says what the copy protocol does with a value of a class: a copy
   hook along its MRO (`__deepcopy__`, `__reduce__`, `__reduce_ex__`, `__getstate__`,
   `__setstate__`) or a `copyreg` registration of the class (`ModuleDecls::copy_registrations`,
   joined into a project index whose changes are class keys) decides it (no report); with none,
   and only known bases (`object`, `typing.Generic`, `abc.ABC`, or a base whose `__deepcopy__`
   the catalog describes as deep, R3), the copy holds deep copies of the attributes; a hook,
   `__getattr__`, `__getattribute__`, or metaclass in a subclass or mixin, or another base
   outside the project, leaves it open.
5. **The join** (`heki_rules::mut_rules::sentinel_copies`): at each deep copy of a checked file,
   for each class along the copied class's MRO (the object is one) and each project subclass
   (it may be one), the sentinels it holds and the tests of that sentinel in that attribute on
   that class, a subclass (known), or a base (open). One report per deep copy, the known claim
   first; the tests are related locations, at most five. What each copied class gives and which
   tests read each class's sentinel are answered once per run.
6. **Below the attributes** (heuristic): the classes an object may hold in its attributes, by
   the attribute types its classes declare (`ProjectDecls::classes_in_attributes`: a class, a
   union's members, a generic container's items one level down, a protocol's structural
   implementations; classes whose copy a hook or a `copyreg` registration decides left out,
   `ProjectDecls::copy_decided_by_class`), read once per class. `heki_ir::walk::Closure` joins,
   per class and once per run, the first class by name and sentinel holding a tested sentinel
   among those it reaches (an unmetered budget: a walk over the declarations expands each class
   once per run, and a budget spent in another order by `heki check` and the language server
   would make them disagree). Each attribute of the copied class gives a heuristic claim naming
   that attribute.

#### `MUT119`: an in-place change lost on a new object (opt-in)
1. **Sources** (`heki_effect::fresh::source`): what makes the object an instruction defines. Built
   as the builtin itself: a list, dict, or set display or comprehension, a call of `list`,
   `dict`, `set`, `bytearray`, `collections.deque`, or `sorted`. Copied: a call the catalog
   describes as a copy (`copy_sanitizer`: a builtin container's `copy()`, `copy.copy`,
   `os.environ.copy()`) whose result is of a builtin container type, called with no argument
   but the value it copies (another may be handed the copy: `copy.deepcopy`'s memo keeps it), or
   a slice of a list or `bytearray`; the type comes from the copied value under R2. Or a call of
   project functions or a property read (its getters), complete when they are every
   implementation it can run. A call of a project class is none: it returns the new instance,
   not what `__init__` returns.
2. **The function fact** (`FreshReturn`, kept beside the summaries in
   `OptInTables::fresh_returns`, local and final, and only while `MUT119` runs:
   `OptInFacts::fresh_returns`; a file without opt-in facts keeps one word): for a plain function
   (not `async`, not a generator, no decorator but those that call the body each time, no A1 hazard)
   with no bare return on a path a run can take, one walk back from the returned values through
   copies and phis (along the edges a run can take) to their sources, all of one container kind, and
   one walk forward from the sources through copies, phis, in-place operators, and the views and
   iterators the object's methods return (`keys`, `values`, `items`, `__iter__`, `__reversed__`):
   every use keeps nothing (a method of the builtin containers whose result holds none of the
   object, `readonly::result_detached_from_container`: a changing method, a count, a test, an item,
   or a copy; an item load, store, or delete on it, not with it as the key, a comparison, a
   conversion, a loop over it, a call of a builtin that only inspects it and returns nothing holding
   it, which `getattr` may); a store, a capture, a yield, a raise, an attribute load (a bound
   method), or any other method or call ends the fact. Both walks share one budget per function.
   `built` says every source is the builtin itself or a complete call or property read.
3. **Propagation** (`Resolver::resolve_fresh_return`): a local fact naming project functions holds
   when each has a final fact of the same kind outside the SCC being computed (a recursive
   function has none), and is `built` only when they all are. Two definitions of one function keep
   the fact only when both have it with one kind. Early Cutoff compares the final facts with the
   summaries (`FinalFacts`), so a change of one re-propagates the callers as a summary change
   does. Both regions of an incremental update hold whole SCCs (`callgraph::scc_closure`), so an
   edited function on a cycle with an affected caller is recomputed with it, as in a full build.
4. **The rule** (`heki_rules::mut_rules::lost_changes`): a changing method of a container kind,
   an item store, or an item delete, applied to a value with no name that nothing reads but the
   change (and the item load of an augmented assignment), whose defining instruction is a source
   of that kind. A method's result must be unused, or `None` bound to names or stored. The tier
   is exact when the object is built, returned by functions whose facts are built and that the
   call resolves completely, or copied where no project subclass replaces the copy or the change
   (R2), and the change is no augmented item assignment; the rule is heuristic until certified,
   so every report is heuristic for now.

#### `MUT112`: a value that is new on one path and borrowed on another, changed in place (opt-in)
1. **What a value holds** (`heki_effect::ownership`): each instruction's `Source` says what the
   object it defines is: new (a display, a comprehension, a copy, a container constructor, a
   project class constructed, an operator's result), held state (an attribute, a global, a
   closure cell), the operand's object (a copy of the reference, `typing.cast`, an in-place
   operator), an element of the operand (an item, `get` / `pop` / `setdefault`, an iteration
   step), an element or the default (`d.get(k, default)`, `getattr(o, n, default)`), or what the
   project callees return. An attribute of an object the function makes itself (through copies,
   `ownership::made_here`, read from the definition alone: whether it makes a new object never
   depends on its operands) is an element of that object, of unknown origin. A parameter is
   borrowed, `*args` / `**kwargs` are new with borrowed elements, and a method's `self` is its own
   object.
2. **The summary fact**: `return_origins` walks back from the returns a run can reach (plain
   sync functions; a memoizing decorator makes the result held, another decorator unknown):
   new, held, parameter `i`, or an element of parameter `i`. A run reaches what
   `ownership::Live` leaves once each branch on a constant takes its one side (`while True:`
   never leaves at its header), the blocks the forward analysis follows too. An object the
   function stores in another object there (an attribute, an item, a display, `setattr`, a
   container method) is an element of that object; one stored in a global or a closure cell is
   held. One walk takes each stored value and the values it copies (through copies and phis)
   with each place once, for all the stores; past a budget of such pairs proportional to the
   function's source, what the function returns is unknown. A project call's result the walk
   reaches is one of `ReturnOrigins::returned.results` (the call's span, and whether the object is the result or
   an element of it), and each argument of such a call gets `ArgOrigin::origins` from the same
   walk: its own origins and, in turn, the calls whose results it may be. A phi the walk meets
   (other than where it starts) is a node of `ReturnOrigins::nodes`, as itself or as an element, walked
   once and referred to by every walk that meets it (`LocalOrigins::nodes`);
   past a budget of walked values proportional to the function's source, what the function
   returns is unknown. Propagation (`returned_results`) adds what those calls return: each
   callee's returned origins without its parameters, and the origins of the arguments bound to
   the parameters it returns (or an element of them), through any number of nested calls
   (`identity(identity(x))`). Each call's result and each node is resolved once, to a fixpoint
   over the calls and nodes it depends on (a loop makes a call or a phi depend on itself): the
   fixpoint only joins, and each change adds a kind or a parameter to one value. A second
   definition of the same qualname appends its nodes after the first one's. The facts take
   part in Early Cutoff like every summary field.
3. **The forward analysis** (`heki_rules`, per function, to a fixpoint that visits each phi and
   instruction once, then again only when a value it reads grows) joins the kinds of a value at
   phis, maps a project call's result through its callees' returned origins and the
   bound arguments, and takes a closure's variable from what the enclosing function bound to it
   (`MakeClosure` captures; functions are analyzed enclosing first). It follows the blocks and
   edges of `ownership::Live` only. A side of an `is` test (of the branch it decides) is the
   edge from the branch to that successor, and every edge from a block that a run reaches only
   through that successor: the walk up the dominator tree from an edge's source to the
   dominator of its target finds them, once per edge, and the test a condition is (through
   `not` and copies) and the value a copy copies are each found once per value. On a phi input
   from the side where an `is` test of the input's value failed, the input leaves out the object
   it is tested against (`if result is shared: result = shared.copy()`). A phi input that copies
   another input of the phi (`ownership::copied_operand`) where a flag of the caller's decides to
   copy it (a branch on parameters other than the receiver and the copied value, and constants:
   `if not in_place: x = x.copy()`) brings the other input's object: the function changes that
   object on purpose where it does not copy it. Where the test is against a constant, a global,
   or an attribute (`if out is None: out = []`) and the phi also takes inputs from the other
   side, the parameter the value is on the failed side is *defaulted*, and the new objects from
   the other side are its replacements: the accumulator idiom. A test that selects no
   replacement (`if p is None: raise ...`) leaves the parameter borrowed. A new object the
   function stores somewhere is part of where it is stored.
4. **Reports**: a change of a container (an item stored or deleted, a container method, an
   in-place operator on a builtin container, a project callee changing the container it is
   handed, `mutated_param_paths` entry `""`) of a value that may be new and borrowed, once per
   value at its first change. A defaulted parameter counts when a new object the value may be is
   no replacement of it (`x = [] if fresh else out` after `if out is None: out = []`), or when
   the value is also stored on `self`. Not reported: a removal when every new object the value may be starts empty, a
   conditional expression changed in place without a name, a value stored back where it borrows
   from (`self.a = self.a or []`, `d[k] = d.get(k) or []`), and memo / cache attributes (as for
   `MUT101`). Stored back means the same attribute of the same object (an attribute borrow keeps
   its object: the method's `self`, a global or an enclosing function's variable by name, else
   the value read, through copies) or an item of the same container: `self.a = other.a or []`
   stores it in another object.
5. **Stored on `self`**: a store of such a value in `self.<attr>` is reported at the store when a
   method of the class family (`ClassMethods::family`) changes that attribute in place
   (`typed_field_writes` with `ContainerMut`, `AliasContainerMut`, or `IndirectHelper` on a class
   of the family); with `MUT112` selected, propagation carries typed writes, and the language
   server re-checks the family of every changed method (`family_readers`).
6. **Only when selected** (`OptInFacts::return_origins`, `heki_rules::opt_in_facts`): the
   summaries carry `return_origins` and `ArgOrigin::origins`, and propagation resolves them, only
   with `MUT112` selected; a run without it computes none of them, and its summaries hold no
   room for them (`return_origins` is `None`). A configuration change re-indexes the language server's
   project with the facts the new selection reads.

#### `MUT113`: containers changed while a method iterates them (opt-in)
The facts are in `EffectSummary::iterations` (`iterations::IterationFacts`, absent for the many
functions with none), with `CallSite::result_held`. The summaries carry them only with `MUT113`
selected (`OptInFacts::iterations`), and propagation copies what callees change in place from
them; a run without the rule computes none of them.

1. Containers: a `self.<attr>` whose class (from the value inferred, an annotation, or the
   `__init__` assignment) is `dict`, `OrderedDict`, `defaultdict`, `Counter`, `set`, or `deque`
   (`typing` aliases included). An abstract `MutableMapping` / `MutableSet`, a union of different
   kinds, and a `list` are not: Python checks these classes' iterators at every step, and a dict
   or set iterator raises `RuntimeError` after its container changed size, a deque iterator
   after any change but an item store or `reverse()`.
2. Iterations (`container_iterations`, a local fact of a method): a `for` loop or a
   `yield from` over the container, its `keys()` / `values()` / `items()` views, or a lazy wrapper
   of either (`iter`, `enumerate`, `zip`, `map`, `filter`, `reversed`, the lazy `itertools`
   functions, a generator expression), through copies and merges; a consuming call (`list`,
   `tuple`, `sorted`, `.copy()`) makes a snapshot. Each records the containers it may step, all
   of them, whether a generator suspends inside it (a `yield` in a block of the loop from which
   its header is reachable without leaving the loop, on normal or exception edges, or the
   `yield from` itself), and the spans of the call sites in such blocks
   (`iterations::BetweenSteps`), once for all the containers. The containers each value may iterate are a
   fixpoint over copies, phis, views, and wrappers, with a pass limit (24) and a budget of
   `(value, container)` facts (one per source byte, at least 4,096, at most 100,000), and the
   iterations take a budget of `(iteration, container)` facts of the same size: past any of them,
   as for a loop rotating aliases of hundreds of containers or one `zip` of thousands of
   containers stepped by as many `yield from`s, the method has no iterations, and no report comes
   from them.
3. Changes of such containers (`changes_self_containers`, propagated through the calls on
   `self`, `super()` included, the property reads, and the helpers changing the container of the
   parameter bound to `self.<attr>`, with the helper's own change of it): `Resizes` for the
   insertion, removal, and reordering methods (`append`, `appendleft`, `extend`, `extendleft`,
   `insert`, `add`, `update`, `setdefault`, `pop`, `popleft`, `popitem`, `remove`, `discard`,
   `clear`, the `*_update` methods, `rotate`, `move_to_end`, `__delitem__`), `del x[k]`, the
   in-place operators (also stored back, `self.s |= t`), and reading a missing key of a
   `defaultdict`; `StoresItems` for an item store (`x[k] = v`, `__setitem__`), which breaks only
   a mapping's iterators. A `defaultdict` read or an item store under a key the container holds
   is no change: on the side of a test that the key is there (`k in`, `k not in`, `get(k)`
   against `None` or by its truth, through `not`) that dominates it and is entered only from the
   test, or after a read of the same key in its block (not for a `Counter`, whose read of a
   missing key inserts nothing). Rebinding the attribute is no change, and neither is a change of
   a read of `self.<attr>` that a store of a new object to `self.<attr>` dominates, when every
   store of the function to it is of a new object: that is the new container's change (a
   callee's change at a call after the store still counts). The key tests, the key reads, and
   the stores (of new objects by the callees of all the function's calls) are gathered in one
   walk per function, and whether one dominates a change is read from the function's dominator
   tree over normal and exception edges. The function's own change sites are
   in `self_container_change_sites`. A function's changes of its parameters' own containers, of
   any class, are counted the same way (`changes_param_containers`, propagated through the
   helpers it hands them to): a helper storing an item of a deque it is handed breaks no
   iterator of it.
4. Calls in a loop's body: a call site among an iteration's calls whose project callees change
   the container while their body runs there (`iterations::changed_at`, through
   `ProjectIndex::delivery`) is reported at the call, with the iteration as a related location.
   A change written in the body itself is the linters' loop-iterator-mutation check.
5. Suspended generators: what can run while a generator method `g` is suspended follows where its
   objects go, from the uses its callers record (`result_iterations`: for each iteration of
   project calls' results in a function, what runs between its steps and the calls whose results
   it steps; one fact for a loop over `zip(a(), b())`, not one per call). At each call of
   `g`, its result is returned (follow the returning function), iterated (the calls in the loop's
   body run; a generator suspending inside the loop is followed), or passed to a project function
   (not followed); whatever else its caller does, it is **held** when it outlives one of these
   uses (`CallSite::result_held`, `iterations::held_results`): it escapes (stored, yielded,
   captured, handed to code heki does not follow, returned by a generator or a coroutine), or a
   step that may leave it suspended (`next()`, a method of it, a `for` loop that leaves its body
   other than through its header or over a wrapper that may stop first, `any()`, a consuming
   call over `islice()`) reaches another use on normal or exception edges. A public function
   followed is **offered**: callers outside the project can iterate it.
   - held: every public method of the class family (its own, its subclasses', the inherited
     ones) counts, except `MUT101`'s two-phase initialization methods;
   - offered: property getters and the read dunders (`__getitem__`, `__len__`, `__contains__`,
     ...) count: a public generator and the public methods changing the container are the
     class's interface, as `dict.items()` and `dict.pop()` are;
   - the family methods the project's loops over `g` call on `g`'s instance count: a call whose
     first argument (a method's `self`) is the object the loop's call binds to the parameter
     holding the instance (`ResultIteration::receivers` and `IteratedResult::arguments`, which
     number these objects within the iteration: the same value through copies, `super()` being
     `self`, or the same attribute of one that runs no property getter). The
     instance is followed through the functions returning `g` or suspending in it as the
     caller's parameter their call binds to it, and is unknown otherwise. A call on another
     object, another parameter included, does not count.
   A method counts when its final `changes_self_containers` holds a change that breaks the
   container's iterators. One report per iteration, at the iteration, naming the first method
   (the class's own first), with each method's first change (its own, else its first call that
   changes the container) as a related location.
6. Incremental updates: `heki_rules::family_readers` re-checks the family of a changed method when
   a class of it has a suspending generator, and the families of the generators whose objects the
   calls of a function may receive when its summary or its call sites changed: its callees (both
   before and after the change, so that a call it stops making is followed too), and in turn what
   each hands on (the result of a call it returns, or iterates while suspending: the relation
   step 5 follows up from a generator). The language server re-checks the files showing a
   container change site that moved, or whose call or change at a place is another one.

#### `MUT103`: cross-function effect snapshots (`heki snapshot summary`)
- The summaries of public functions (`mutates_params`, `escapes_params`, `mutates_self_attrs`,
  and `direct_exec_effects ∪ transitive_exec_effects`) are saved in a normalized form
  (`.heki-summary-snapshot.json`); a pull request whose functions gained effects or mutability is
  reported.
- Entries are built and compared from each function's defining file (`ProjectIndex::file_of`:
  the `.py` file before its `.pyi` stub), and `--write` replaces the file atomically only when
  every file of the project was read and parsed.

#### `MUT106`: single-pass iterators consumed twice (opt-in)
- Sources: generator expressions; `iter` / `zip` / `map` / `filter` / `enumerate` / `reversed`
  and the single-pass `itertools` functions (never `tee`, `repeat`, `cycle`, `count`); project
  generator functions; and `Iterator` / `Generator` annotations (not `Iterable` / `Sequence`).
- Consumers: `for` loops and comprehensions, unpacking, `yield from`, and the calls that
  materialize an iterable (`list` / `tuple` / `set` / `frozenset` / `dict` / `sum` / `min` /
  `max` / `sorted` / `str.join`, `list.extend`, `set.update`, `collections.deque`,
  `heapq.nlargest`, `functools.reduce`, ...: one table, `consumption::FULL_CONSUMERS`, which also
  decides where a generator's body runs), also through wrappers over one iterable; `any` / `all`
  step it but may stop early (`STOPPING_CONSUMERS`: a generator's body runs, the iterator is
  not exhausted); `next()` is
  stepping, not consumption. A project function consumes its argument when `consumes_params` says
  so, and its body runs at the call.
- The first pass must be full (a loop that can `break`, `return`, or `raise` is partial); the
  second pass must be reachable from it on normal CFG paths; an iterator created outside a loop
  and consumed inside it is reported unless the loop runs at most once.

#### `MUT107`: attributes `__hash__` depends on
- `reads_self_attrs` of `__hash__` (with its `self` helpers) against the own writes of every other
  method of the class and its subclasses, with `MUT101`'s initialization, memo, and constant-key
  filters, writes directly under a `None` / falsy guard of the same attribute (lazy
  initialization, `CacheFlow` writes), the attributes `__hash__` caches itself (`self._hash`), and stored-hash names.
- Initialization includes the private methods only `__init__` and such methods call (every
  caller in the reverse call graph is one): `__init__` -> `self._load()` -> `self._decode()`.
  Since any method's calls decide this, the language server re-checks a class family with a
  `__hash__` or a cached property when any of its methods changes (`family_readers`).
- A writer whose class uses another `__hash__` (`ProjectDecls::hash_owner`: a class between it and
  the hash's class defines `__hash__`, or `__eq__` alone, which leaves its instances unhashable)
  is not paired with that hash.
- `hashed_whole_self_attrs` of `__hash__`: the attributes every read of which is handed, as
  itself, to `hash()` (alone or as an element of a hashed tuple display) or has `__hash__` called
  on it, read by no `self` method or property the hash calls, with `self` handed to no other code.
  An in-place change of such an attribute (an item store, a mutator method) changes only the
  value's own hash: a builtin container that can change in place cannot be hashed, and a project
  value's own `__hash__` is checked in its own class. Rebinding the attribute is still reported.

#### `MUT201`: declared write ownership
Declared in `pyproject.toml` (no decorators, no run-time overhead):

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = [
    "datamodel_code_generator.model.base.DataModel.fields",
    "datamodel_code_generator.model.base.DataModel.base_classes",
    "datamodel_code_generator.types.DataType.data_types",
]
allowed-writers = ["datamodel_code_generator.parser.generation.*"]
exempt-methods = ["__deepcopy__", "copy_deep", "create_reuse_model"]
# Optional: a @cached_property defined on these classes is reported too.
forbid-cached-property-on = ["datamodel_code_generator.model.base.DataModel"]
message = "write model state through GenerationStore instead"
```

- **Algorithm** (removes the weaknesses of datamodel-code-generator's
  `check_generation_store_usage.py`, local aliases and indirect helpers, 13 misses, with the
  SSA-CFG and the call graph):
  1. Every operation on attribute `a` of a target class `C` (and its subclasses) is a **write
     origin**:
     - reassignment, deletion, and augmented assignment (`obj.a = ...`, `del obj.a`,
       `obj.a += ...`);
     - `setattr(obj, "a", ...)` / `object.__setattr__(obj, "a", ...)` /
       `obj.__dict__["a"] = ...`, the namespace being `obj.__dict__`, `getattr(obj, "__dict__")`,
       or `vars(obj)` (the builtin: `resolver::namespace_load`, which `MUT101`'s invalidations
       read too); `update`, `popitem`, `clear`, and `|=` on it write names heki cannot tell;
     - **container mutation through an alias**: `xs = obj.a; xs.append(...)` / `xs[i] = ...` /
       `xs.clear()` / `xs.sort()` / `xs |= ...` (through alias tracking and
       `mutated_param_paths`, **handing `obj.a` to `helper(obj.a)` that calls `.append()` is
       caught across calls**). An augmented assignment that changes a built-in container in
       place (`types::changes_in_place`: `+=` on a list, `|=` on a dict, `-=` on a set) gives
       the container itself, so the name it rebinds stays the alias and keeps its type
       (`xs |= {}; xs["k"] = v`); on any other value (`n += 1`) it binds a new object. An alias is one in the scopes nested in its function too: a
       read of its closure cell joins the aliases of every value the function binds to it, as
       a phi joins its inputs (the function is summarized before the scopes nested in it), and
       a cell a nested scope rebinds (`nonlocal`) is no alias. `getattr(obj, "a"[, default])`
       with a constant name loads
       `obj.a`, and so does a call or property read whose every project callee returns
       attribute `a` of the parameter the object binds on every `return`
       (`FunctionDecl::returns_param_attr`: `node.members().append(1)` and
       `node.view.append(1)` with `return self.items`; never a `cached_property`), when every
       class the receiver may be takes the name from one of those callees: along its MRO, the
       first class binding the name defines one (`ProjectDecls::function_taken`).
  2. `typed_field_write_sites` propagate bottom-up with their call chains, from private callees
     only: a public callee reports its own writes. A write is allowed
     when a function on its chain matches `allowed-writers` or `exempt-methods`, when an
     initializer of the target's class family writes its own `self`, or when the reporting
     function only runs inside allowed contexts (every path of project callers starts in one); a
     private helper with project callers hands its report to them. Otherwise the outermost
     function is reported, with `message` as the report's help. With
     `unlisted-dict-keys = "report"`, a `dict_direct` site that hits no target, on a class some
     targets cover, whose key no target of any `write-owners` contract covering the class names
     (`NamedDictKeys`, built once per run from every contract's targets), is judged the same way
     once per family of the contract's targets covering the class (`OwnerRule::written`), and
     reported as that key (`Class.key`). With `unresolved = "report"` too, a `dict_direct`
     unresolved write site (an object of unknown class) whose key no target names for some class
     of the contract's targets is `MUT205`'s (`unlisted_key_classes`): a caller binding one of
     those classes leaves the write there, as the caller's `IndirectHelper` site carries no
     `__dict__` kind for `OwnerRule::written` to judge.
  3. Existing code bases migrate with **`heki check --baseline`**: counts are frozen per
     `(path, qualname, "MUT201", "Class.attr")` and can only shrink.

#### `MUT202`: declared read-only returns
Declared in `pyproject.toml`:

```toml
[[tool.heki.readonly-returns]]
name = "shared-schema-graph"
producers = [
    "datamodel_code_generator.parser._schema_graph.SchemaGraphBuilder.schema",
    "datamodel_code_generator.parser._schema_documents.load_document",
]
deep = true   # changes to attributes and nested containers below are forbidden too
```

- **Algorithm**:
  1. The SSA value returned by a call to a `producers` entry (or to an `@lru_cache` / `@cache`
     function) gets a read-only origin tag.
  2. The tag flows within the function through `Copy`, attribute reads (with `deep = true`),
     subscripts, and tuple / list unpacking; copy sanitizers end it (`copy.deepcopy`, `.copy()`,
     `model_copy(deep=True)`, `dataclasses.replace`, `copy.replace`, the builtin container
     constructors `dict` / `list` / `tuple` / `set` / `frozenset`, and the Layer 2
     `copy_sanitizer` entries, as deep as each entry says: a shallow copy ends it for the top
     level only), a display that only unpacks tagged values (`{**x}` with no other entry,
     `[*xs, *ys]`, `readonly::display_copies`) is a shallow copy of them, and a slice is a shallow copy unless
     the value's class is inferred and is not a builtin sequence
     (`readonly::subscript_step`, shared with the return flow). `readonly::copy_of` is the one
     table of the copies an instruction makes of the values it is given alone, which
     `MUT112`'s origins (`ownership::source`: a shallow copy is a new object holding the copied
     values' elements) and the summaries' copies of parameters read too. A copy method of a project class
     (`Image.copy`) is a call of that method, not a copy by its name (`readonly::call_derivation`).
     A copy keeps its original's class for receiver typing. A phi joins the tags of all its
     incoming values, to a fixpoint.
  3. A direct attribute write or container mutation of a tagged value, or passing it in a
     parameter position the callee changes (`mutated_param_paths` minus `memo_param_paths`; for a
     shallow copy, below the attributes `changed_param_attrs` names; for a producer that is not
     deep, not below its items), is reported as **`MUT202`**
     with the call hop, or as **`MUT114`** when the value comes from an `@lru_cache` / `@cache`
     function rather than a declared producer. A `MUT114` report is exact when the cached
     function carries a `CachedResult` (a `functools` cache resolved by import, with a `maxsize`
     other than `0`, a parameter besides `self` / `cls`, and a builtin list, dict, or set on every
     live return, by its return annotation or by construction), the changed value is that one
     function's result on every path (`MustResult`, an optimistic fixpoint over copies and phis:
     a value starts unset and moves only down, to one cached function, then to anything else),
     and the instruction changes a container of that kind in place. One decision,
     `Mut202Fn::cached_change_steps`, tells each premise that does not hold, which a heuristic
     report records as open when the run asks (`CheckExtras::open_premises`): "Same object" from
     `MustResult` (over the result of any one project function, so that the cached function's
     own premises stay apart) or a wrapper on the way, "Cached", "Keyed", and "Container kind"
     together from a missing `CachedResult` (it keeps no reason, so all three are recorded), and
     "Changed in place" from a change through a called function, a line that is no change of the
     kind, or a subclass the annotation admits that replaces the method. Filling a memo of the shared object or initializing one of its attributes
     lazily (`SelfAttrWrite::fills_memo`, `lazy_init`, on any object a parameter or a tagged value
     holds; through a call, a change of the object a memo-named attribute holds) is no change:
     what the object computes stays the same. A memo fill is a store under a test finding its key
     missing (`not in`, the `else` of `in`, a `None` or falsy `.get(k)`, through
     `guards::unset_branches`; the key compared by `cache_flow::same_key`), `setdefault`, or a
     change of a memo-named container (also a parameter's default container); a lazy
     initialization is a store (`guards::stored_attr`: also `setattr`, `__setattr__`) in a block
     only a branch finding the attribute unset leads to (`guards::attr_tests`: `None` and
     truthiness tests of `obj.attr`, `getattr(obj, "attr", ...)`, `hasattr(obj, "attr")`, and the
     `except AttributeError:` clause of a `try` whose body reads it). A change below a shallow copy
     of a parameter (`copy.copy(p).tags.append(x)`) is a change of the parameter.
  4. **Return flow**: a backward walk from the returns a run can reach (`ownership::Live`: not
     a `return` after a `while True:` loop that no `break` leaves) marks each call whose result
     may be returned (as itself, as an element, or as an item of target unpacking; a copy ends
     it). A callee that is a producer, an `@lru_cache` function, or `returns_protected` makes
     the caller `returns_protected`, so a wrapper returning a producer's result is a producer
     itself, across modules. The join is "protected if any path returns it", and the shortest chain is kept.
     Only plain functions are followed: a generator's `return` value is not what calling it
     returns.

#### `MUT109`: announced changes
- `[[tool.heki.invalidation]]` (`targets`, `invalidators`, `exempt-writers`, `message`):
  extraction records the target writes whose object is reached from parameters or globals, and
  the direct invalidator calls. A greatest-fixpoint coverage dataflow over normal edges (raising
  paths count as covered), with constant and boolean-flag folding, gives each write whether every
  normal path from it reaches an invalidator.
- Propagation gives `must_invalidate` and `unmet_invalidation` through the `next_call` chains and
  the callees' `must_invalidate`. A callee's writes and announcements are taken where its body
  runs (`ProjectIndex::delivery`): a plain function at its call, a generator where it is iterated
  or inside the function it is handed to; only bodies run at their call count as announcing.
  A write a callee makes through an argument it does not type (an in-place change of `obj.attr`,
  an attribute of `obj`) is the caller's write at the call: the caller's argument binding ties
  it to a class (`ArgOrigin::targets`, the same `callgraph::indirect_writes` the ownership rules
  use), or to the classes implementing the protocol the argument is typed with
  (`ArgOrigin::implementers`, `ArgOrigin::instance_classes`). The written object's identity (`obj_params`, the parameters it may be as itself) decides
  an initializer's own construction: its `self`, not the objects reached through `self`.
- Reporting: private helpers never report and pass the obligation to their callers; a public
  function is relieved when every caller path announces the change after the call (a
  breadth-first walk over the callers; exempt callers count).

#### `MUT203`: declared read ownership
- `[[tool.heki.read-owners]]` (`targets`, `allowed-readers`, `exempt-methods`, `message`): reads of
  the targets (attributes, local aliases, constant-name `getattr` / `object.__getattribute__`,
  `__dict__["a"]` / `__dict__.get("a")`) outside the allowed readers. Read sites are local facts;
  the rule walks the reverse call graph from private helpers to their outermost non-private
  callers, with the same initializer and allowed-context rules as `MUT201`. Reads through
  parameters without a project-class type are not seen.

### 2.4 Family 2 (`EFF101`–`EFF103`, `EFF105`–`EFF107`, `EFF109`, `EFF110`, `EFF111`, `EFF112`, `EFF113`, `EFF114`, `EFF115`, `EFF116`, `EFF201`, `DATA101`): precision, sanitizers, and aggregation
0. **Must-facts and the exact tier** (`EffectSummary::must_exec`, computed only when `EFF101`,
   `EFF111`, `EFF103`, or `EFF113` runs: `OptInFacts::must_effects`):
   - Per call instruction, the path facts (`heki_ir::Paths`, with the reader of
     `heki_effect::with_paths`: the module's bindings, the members of modules, and the context
     managers whose exit never suppresses, `exit_suppresses`) say whether the function has made
     the call whenever it returns normally (`CallSite::on_every_return`: made, returned, or
     reachable at all). A call of a class body the function runs is made or returned whenever
     the function returns only when the body's own paths say so and the function has run the
     `class` statement to its end whenever it returns (a nested class's statement through the
     statements around it; a statement a `finally` body repeats, through one of its copies), and
     reachable only when the statement is.
   - A call's targets are complete (`CallSite::complete`) when they are every implementation
     the call can run under R2 and R5: for each of the receiver's class and its project
     subclasses (at most `DISPATCH_CLASS_LIMIT`, 64), the definition its MRO finds is a target;
     every member of a union resolves; a protocol receiver never is complete. A call of a
     property's value is complete when calling a value of the type the getter declares is
     (`TypeCtx::targets_complete_on`), whatever the receiver. A read of a property, the call of
     its getters, is complete when each class the object may be (at most
     `DISPATCH_CLASS_LIMIT`) takes the name from one of them (`ProjectDecls::function_taken`: the
     first class along its MRO that names the name defines it as that getter, no class before it
     stores the name on the class object outside its body, and no base heki does not know comes
     before it), and none defines `__getattribute__` or has a metaclass heki does not know
     (`TypeCtx::getter_targets_complete`). A class that finds a class attribute, a method, or
     nothing first runs no getter. Through `super()`, each class takes the name from one of them
     after the class `super()` is in, along its own MRO (`ProjectDecls::super_getters_complete`).
   - A Layer 2 call has an effect unconditionally (`CallTarget::Layer2::unconditional`) when its
     entry `emits` it on every call and no case of the entry tests an argument that may be a
     parameter of the caller (R6 covers the arguments heki cannot evaluate).
   - A function has run effect `e` whenever it returns when it makes, on every normal return,
     an unconditional Layer 2 call with `e` (made, also if it raised), or a complete call whose
     every non-abstract target runs at the call, as written (decorators that call the body each
     time), on the caller's thread, and has run `e` whenever it returns (returned). A target
     that never returns normally has run anything; a name bound twice has no facts. The facts
     grow over each SCC with the summaries and take part in Early Cutoff.
   - `EFF101` reports a boundary when its call has run a wait for input or output
     (`ExecEffectKind::wait_kind`) whenever it returns, through synchronous targets
     (`ProjectIndex::site_must`); `EFF103` when the call runs on every iteration that completes
     of a loop over a parameter, a part of one, or a catalog call's `items`, whose trip count the
     code does not fix (`heki_ir::ItemOrigins`, `FnIndex::fixed_trips`), and has sent a statement
     whenever it returns. Everything else the heuristic tier finds is `EFF111` / `EFF113`.
   - Open premises (`CheckExtras::open_premises`, off in `heki check` and the language server,
     on in `heki recommend` and the hidden `heki dump-open-premises`): an `EFF111` / `EFF113` report records which
     premises of its claim's row (`heki_rules::premises`) it left open, each with a cause. The
     decisions above tell each step that does not hold (`MustGap`) to whoever asks: a decision
     stops at the first, an explanation goes on. Along the reported chain each call is evaluated
     (`MustExplainer::gaps_along`): its targets' completeness, whether they run there as
     written, their facts, and, below the first call, whether the caller makes the next call on
     every normal return (`CallSite::made_on_every_return`); the walk goes down while the
     function the chain enters lacks the fact, whose own gap then gives way to the gaps found
     below it. For `EFF103` the loop premises are evaluated on every loop around the call, and
     the loop that leaves the fewest open (the first such) gives them. A cause stands for the
     cases its facts cannot tell apart yet: `CallSite::complete` keeps no reason, so a protocol
     receiver, an unknown base, more than 64 classes, and an unresolved union member are one
     cause, and `CallTarget::Layer2::unconditional` keeps none either (it is computed only for a
     complete call, so at an incomplete last call "Waits" / "Sends" is not evaluated).
1. **Offload wrapper propagation** (from datamodel-code-generator `bodies.py:112`):
   - When a function `w(fn, *args)` calls `asyncio.to_thread(fn, ...)` (`offloads_blocking_arg =
     0`) or similar, the parameter index of `fn` is recorded in `w`'s `offloads_blocking_params`
     and propagated upward, which stops `await in_thread(self.open)` from counting as blocking.
   - An offloading decorator (`@sync_to_async`, `@database_sync_to_async`, `@asyncify`, or a
     project decorator declared with `offloads-blocking-arg = 0`) makes calling the function a
     call of the decorator's catalog entry (section 2.2).
2. **Sync-over-async relaxation** (from datamodel-code-generator `client.py:1399`):
   - A private `async def` without route decorators that is only driven by synchronous drivers
     (`.send(None)`, `run_sync`) and never awaited from the event loop is `sync_driven_only` and
     relaxed.
3. **Stage 0-A filters**:
   - **`EFF101`**: calls with `is_awaited == true` excluded; `subprocess.Popen(...)` itself (a
     non-blocking spawn) excluded, while `.wait()` / `.communicate()` stay blocking.
   - **`EFF102`**: `for.iter` headers excluded; reconnects in `DaemonPolling` loops relaxed; a local
     dict named `requests` is not mistaken for the package (type and name resolution). A builtin
     calling a function once per item (`map(fetch, urls)`, `sorted(xs, key=f)`) is a loop over
     its items, relaxed as a `for` loop over the same iterable.
   - **`EFF103`**: `for.iter` headers excluded; a DB-API `execute` / `executemany` /
     `executescript` call (or SQLAlchemy's `exec_driver_sql`) whose statement is DDL (`CREATE`
     / `ALTER` / `DROP` / `PRAGMA`) records no `DbQuery` where it runs (it still blocks), so
     helpers running DDL run no query either. The statement is DDL when every text it may hold
     starts with such a keyword (`summary/statements.rs`): a string constant, the literal start
     of an f-string (`IrFunction::str_heads`), of a `+` / `%` with a constant first operand,
     or of `.format()` on one, when the keyword ends inside it; `strip()` / `upper()` ... of
     one, SQLAlchemy's `text()` of one, each input of a phi, and each item a loop over a tuple
     display, a module constant tuple, or the pieces `.split(sep)` / `.splitlines()` cut a
     module constant string into (`ModuleConstants::strs` / `str_tuples`). Queries in
     migration functions excluded; the internals of ORM libraries excluded.
   - **`DATA101`**: only `df = pd.concat([df, ...])` / `arr = np.vstack([arr, ...])` whose SSA
     `PhiNode` accumulates across the back edge (`LoopCarried`), or a project call there whose
     callee's `reallocates_params` covers the argument carrying the previous value; one-off
     concatenation and padding in a loop are excluded, and so is a value rebuilt only from
     arithmetic on the previous one (`x = np.concatenate((x - d, [a]))`): an operator holds an
     operand only as list or tuple concatenation and repetition or dict union, and an operand
     sliced or computed from the previous array or frame makes it work entry by entry. The exact
     tier is one decision (`UpdateDecision::steps`), told the premises that do not hold in the
     order it takes them: the update is a catalog call that reallocates on every call
     ("Reallocates"), the previous value is held and every path around the back edge carries
     the update ("Loop-carried on every path"), every iteration, and the loop's trip count and
     items ("Origin", as for `EFF103`). A decision stops at the first and asks for the loop's
     items whatever the steps before it found, as it always did; an explanation (when the run
     records open premises) goes on with a budget the function's explanations share.
4. **Duplicate aggregation (boundary-only reporting and root-cause grouping)**:
   - In `async A → await async B → sync C → blocking D`, only the first crossing into the
     synchronous world (`async B`) is reported; when more than a threshold (10) of the reports
     shown reach the same root cause, output folds them into one representative with the count
     (`heki_rules::fold_root_causes`). Folding comes after inline suppressions and the Ratchet
     baseline, which see every report.
5. **Context relaxation**: `click` / `typer` commands (`@command`), ASGI lifespan hooks,
   `LoopBound::ConstSmall` / `RetryPattern` loops, and test code (`lint.relax-async-in-tests`,
   default true) are relaxed by default.
6. **`EFF106`** (opt-in): a forward must-hold dataflow over the CFG, one fact per acquiring
   `with` item (an `ExitWith` releases the acquisitions of its own statement), for `with` regions of
   synchronous locks (Layer 2 `sync_lock_or_tx` classes: `threading.Lock` / `RLock` / `Semaphore`
   / `BoundedSemaphore` / `Condition`) and transaction starters (`Session.begin()`,
   `Connection.begin()`, `Engine.begin()`, `transaction.atomic()`, `psycopg.Connection.transaction()`,
   ...); an `await` or a call site reaching `BlockingNet` inside the region is reported.
7. **`EFF107`** (opt-in): a `create_task` / `ensure_future` result bound to a named local is
   reported when no alias of it, nor a method bound to one (`task.cancel`, which holds the task),
   is awaited, returned, yielded, stored, put in a container, passed to a call, or captured;
   being only the receiver of `add_done_callback` / `set_name` does not keep it, `cancel()` /
   `result()` / `exception()` do. A call whose argument (`coro` / `coro_or_future`) is known not
   to be a coroutine makes no new task and is skipped: the result of project functions that
   all create none (`FunctionDecl::makes_no_coroutine`: a plain `def` or a generator function
   that no decorator replaces, `FunctionDecl::called_as_defined`; never a class's `__init__`),
   or a value typed as one of `heki_kb::FUTURE_CLASSES` (an annotation, or a Layer 2
   `return_type` such as `loop.run_in_executor` / `asyncio.gather` → `asyncio.Future`).
8. **`EFF109`** (opt-in): a non-reentrant lock taken again while it is held.
   - A lock is an instance of a Layer 2 class flagged `non_reentrant_lock` (`threading.Lock`,
     `asyncio.Lock`, `asyncio.Condition`, `multiprocessing.Lock`), named by where the function
     reaches it (`heki_effect::locks::ObjRef`): parameter `i` or its attribute (`self._lock`), a
     module global (by fully qualified name, read by name, through imports, or on its module),
     or a variable of an enclosing function its closures capture (typed from what that function
     binds to it, `TypeCtx::captured`). An acquisition is a `with` / `async with` over
     it, an awaited `acquire()` without arguments, or a blocking `acquire()` the catalog lists
     (`threading.Lock.acquire`) without a timeout or `blocking=False`.
   - The facts are in `EffectSummary::locks` (`locks::LockFacts`, absent for the many functions
     with none) and `CallSite::state_tests`; the summaries carry them only with `EFF109` selected
     (`OptInFacts::locks`), and a run without the rule computes none of them.
   - The tests of state on every path to a block (`locks::BlockTests`, one pass over the
     dominator tree of the edges a run can take): the attributes of the parameters, the project
     methods called on a parameter, and the module globals that the conditions of the branches
     ending its dominators read; at most 16 tests, past that the answer is open
     (`heki_ir::walk::Answer`) and every test may be one. Each call site keeps those of its block
     (`CallSite::state_tests`).
   - `LockFacts::acquires` holds the function's own acquisitions (a hop to the lock's class at
     the acquisition), each with the input guards every acquisition needs and the tests of state
     on every path to every acquisition of the lock (those of the acquisitions' common
     dominator): the module globals, and, for a lock a parameter holds, the attributes of that
     object and the project methods called on it (`LockAcquire::tests`; open past 16 tests,
     where the acquisition counts as guarded). Propagation adds the acquisitions of each callee whose body runs at the call on
     the caller's thread or task (not on a worker), with the lock mapped through the argument
     bound to the parameter, the call in front of the chain, the guards mapped by
     `guards::through_call` (a constant argument deciding one against it drops it), and the
     tests of state of the call site added; several acquisitions of one lock keep the guards and
     tests all of them have and the shortest chain.
   - `LockFacts::rebinds` holds the module globals and closure variables the function may bind
     or delete (`global`, `nonlocal`, an attribute of a module, a variable its closures capture),
     itself or through its calls; `mutated_param_paths` has the attributes of parameters. A
     callee that may bind a lock's name takes no lock under that name at the call
     (`locks::may_rebind`): the name may reach another object by then.
   - The rule runs a forward must-hold dataflow (`heki_effect::held`, shared with `EFF106`: a bit
     per acquisition, an `ExitWith` giving back those of its statement, a `release()` the
     acquisitions of its lock, also a `with`'s, and binding a lock's name, by the holder or by a
     call whose callees may bind it (`ProjectIndex::site_rebinds`), those of that name) and
     reports, while a lock is held, its own acquisition again and a call site whose callees take
     it (`ProjectIndex::site_locks`), unless that acquisition is behind a test of state the
     holder changes (an attribute of the lock's owner, tested by the acquiring function or by a
     function on the way, that the holder's summary changes, `mutated_param_paths`,
     `mutates_self_attrs`, a tested method counting with the attributes it reads; a module global
     the holder stores; the holder's own tests do not count), or some function of the module
     releases the lock where it does not hold it (a signal between threads).
9. **`EFF110`** (opt-in): an object allowing one operation at a time, used by tasks started
   together.
   - A use is a call of a Layer 2 method flagged `one_at_a_time` on an instance of its class, or
     an `async for` over one (`__anext__`): `"always"` (`asyncio.StreamReader.readline`,
     asyncpg's `Connection.fetch`, websockets `recv`, aiohttp `receive`) or, for an object that
     raises only while it holds no connection, `"when_unbound"` (SQLAlchemy's `AsyncSession`
     operations) and `"unbinds"` (`commit`, `rollback`, `close`).
     `EffectSummary::exclusive_uses` holds the function's own uses and those of the calls running
     in its body, in its own names (`ObjRef`, mapped through the arguments as for
     `acquires_locks`); a use while the function certainly holds a lock (`locks::lock_steps`,
     `held::at_entry`), and the uses of a call made while it holds one
     (`CallSite::under_lock`), are left out.
   - The rule (`eff_rules::fanout_exclusive`) finds the fan-outs of a function: the arguments of
     `asyncio.gather` (also a generator expression or a comprehension unpacked into it, whose
     element is started once per item), the display `asyncio.wait` waits on, and the
     `create_task` / `start_soon` calls of one task group (in a loop, started once per
     iteration). A task is a coroutine a project call creates, `asyncio.create_task` of one, a
     call a task group starts, or a one-at-a-time call. Its callees' uses map through the call's
     receiver, positions before any `*` unpacking, and keywords to what the fan-out function
     names: a parameter or its attribute, a global, a closure variable, or a local variable
     (its value before copies, or an attribute of one); a generator expression's own values are
     per item, the variables it captures the function's, and a value the loop starting the task
     defines is per iteration. Two tasks, or one started several times, on the same object are
     reported. For `"when_unbound"` objects, a one-bit must-dataflow over the function (gen at
     an operation on the object, its own, an awaited call reaching one, or a fan-out whose
     tasks use it; kill at `"unbinds"`) leaves out a fan-out that every path reaches after a
     use.
10. **`EFF112`** (opt-in): the iteration order of a set of strings reaching an output across a
   call.
   - `heki_effect::order_flow` decides, per function and IR-free afterwards, which sources reach
     which ends. A source is a take of a set's order (the elements of a loop or comprehension over
     the set, `list()` / `tuple()` / `iter()` / `str.join()` / formatting of it, `set.pop()`, a
     generator expression over it, a comprehension of its own before Python 3.12 handed it; a set
     the code bounds to one element on every path to the take, by a test of `len` whose bounding
     edge is the only way in, also an operand of a short-circuit `or` / `and` on the side of the
     whole that decides every operand, takes none), a parameter, or a project call's result. The set has
     run-dependent hashes when it is a `set` / `frozenset` of `str` or `bytes` (or tuples of
     them): a display of text, `set()` / `frozenset()` of text, a set operator with such an
     operand (a mapping's `keys()` of text keys included), a module name bound once to such a
     display (`TypeCtx::string_set_global`), a phi of one, or a value whose inferred type is one
     (one memoized answer per value). The flow is a `Closure` over `(value, kind)` nodes: a whole
     value in the set's order passes through copies, phis, list / tuple / dict displays,
     formatting, `+` / `%`, `str.join`, the builtins that keep an order, `.items()` and the other
     keeping methods, positional item reads (not from a mapping, by a text key, or from a
     sequence the code bounds to one element there), and into the containers keeping the order
     it is appended to or stored in (not a set, which has an order of its own; `insert`,
     `update`, and `setdefault` only on a list or a mapping known by construction or type; a key
     read from the mapping on every path before keeps its place when stored again); an element
     also passes to what is computed from it (an item looked up by it, text formatted or
     concatenated from it, a call's result), and text made from it and added onto a phi (what a
     loop has built so far) is a whole value in the set's order. The
     ends are a `return`, an output (a catalog `writes_in_order` argument with its sorting
     keyword established off, a stream's `write`, a builtin exception's message; for one element
     at a time only outputs that append: streams and `print`, not a raise, which names the first
     element that fails), and a project call's argument (with the callee and parameter; `each`
     for one element at a time).
   - Propagation (`order_flow::propagate`) resolves the flows against the callees' facts: what a
     call's result carries (the callee's `returns`, or, through a callee returning a parameter,
     what the argument carries: a fixpoint over the flows that visits a flow again only when it
     grows; one element at a time hands no order back), then `returns` (the first origin),
     `returns_params`, and `writes_params` (the shortest chain to an output; through an `each`
     argument only to an appending output).
   - The rule (`eff_rules::set_order_output`) reads the same resolution where the ordered value
     meets the output: a call's result written (it came back across a call), or a take handed to
     a call whose parameter reaches an output. A take written in its own function is a linter's
     and is not reported.
11. **`EFF114`** (opt-in): an `except` clause that turns the `RecursionError` of a call cycle
   into a value.
   - A function is on a call cycle when it is a member of a component, of more than one node or
     with a call of itself, of the propagation region's call graph restricted to the calls whose
     callee's body runs on the caller's stack (`callgraph::on_call_cycles`: a call that runs at
     the call, not a coroutine only created or a generator only made, or a generator iterated
     there; never a callable run on a worker thread). The components are computed once per
     propagation; a region always holds every member of a cycle it touches (an edit's region
     holds every function sharing a cycle with it, a caller region every caller of a changed
     function). `EffectSummary::call_cycle` is `On` for a member and `Reaches` for a function
     whose delivered callee facts reach one (joined during propagation like the other facts, so
     a generator's or a returned generator's iteration counts where it runs), and takes part in
     Early Cutoff.
   - The rule (`eff_rules::recursion_handlers`) reads, per `try`, the clauses its dispatch block
     tests in turn (the `CatchException` that starts each clause body) and takes the first whose
     classes include `RecursionError`: a bare `except`, or class values the type inference gives
     as `builtins.RecursionError`, `RuntimeError`, `Exception`, or `BaseException` (a tuple
     display of them included). A class is known when it is a builtin (a name Python binds in
     `builtins`) or a project class statement, which makes a new class; an earlier clause of
     any other class (one of a dependency or the standard library, which may be bound to
     `RuntimeError`, as `threading.ThreadError` is, or a name only a star import may bind),
     of a value heki does not know, or an `except*` clause (by its source text), leaves the
     `try` undecided. The `try` body is every block whose
     exception reaches that dispatch block, past `with` exits and past the dispatch of an inner
     `try` none of whose clauses takes it (one walk per exception target). A call site in the
     body whose delivered callee facts reach a call cycle (`ProjectIndex::delivery`, into the
     body; never a worker thread) is the witness. When the witness's callee only reaches the
     cycle, a walk down its calls (`reach`, under the function's budget; past it, no finding)
     ends at a function on the cycle or at one an earlier walk of the module's check decided,
     and each function on the way keeps its first call down: every function's call is decided
     once for all the chains through it, and a report lists the first eight calls of its chain. The clause completes normally when a walk over
     the blocks the start of its body dominates, along normal edges, finds no `raise`, no call ending
     the process (`sys.exit`, `os._exit`, `os.abort`, `exit`, `quit`), and no call of project
     functions that return on no path (`MustExec::never_returns`, which `EFF114` has computed);
     leaving those blocks or
     returning completes it. A clause reporting the error (`print(..., file=sys.stderr)`,
     `sys.stderr.write`, `traceback.print_exc`, a `logging` call with the exception, also on a
     logger of unknown class by its method name) and returning an integer constant on every
     `return` is a CLI boundary and is left out.
12. **`EFF115`** (opt-in): an `async def` handed to a library call that never awaits what it
   returns.
   - The summary (`summary::Extractor::unawaited_handoff`, only with
     `OptInFacts::unawaited_handoffs`) records, at each call of a catalog API with a callable
     argument it calls without awaiting (`EffectiveEcosystemKb::unawaited_callable`: the API's
     `[[calls_unawaited]]` entry, or its entry's `offloads_blocking_arg`), the project functions
     the argument calls
     (a `functools.partial` object followed to the function it holds, a bound method resolved to
     the implementations it may run; a class constructs instead and is left out), in
     `OptInTables::unawaited_handoffs` (`FileSummary::opt_in_tables`) by calling function, the
     class bodies a function or the module body runs included. The record holds every such function, `def` or `async def`.
   - The rule (`eff_rules::unawaited_callables`) reads the declarations of the recorded
     functions at check time (`FunctionDecl::is_async`, `is_generator`, `called_as_defined`), so
     a change of a function between `def` and `async def` in another module re-checks the
     caller's file through its declaration reads. It reports when one of them is an `async def`
     that is no async generator, worded as certain when every one is an `async def` no decorator
     replaces, and as possible otherwise. Only catalog APIs count: a project function taking a
     callable resolves to the project and is never one, whatever it does with the coroutine.
13. **`EFF116`** (opt-in): a call of a project generator function whose generator nothing
   iterates, so its body does not run.
   - The rule (`eff_rules::never_iterated`) reads each function's IR: a call (not awaited)
     whose result has no name and whose every reader is a truth test (a branch's condition,
     `not`, or a call of the builtin `bool` the resolver confirms) is a candidate, one with no
     reader at all a statement. Only a function with a candidate is typed (`TypeCtx::infer`);
     the call is resolved there (`resolve_call`, `resolve_method`) and judged complete as
     `CallSite::complete` is (`TypeCtx::inst_targets_complete`). The callees' declarations are
     read at check time (`FunctionDecl::is_generator`: a generator function no decorator
     replaces), so a change of one between a generator and a plain function in another module
     re-checks the caller's file.
   - The hand-off form reads `EFF115`'s records (`OptInTables::unawaited_handoffs`, computed
     when either rule runs) and skips the APIs that hand the result back
     (`UnawaitedHandoff::hands_back`); each record also says whether its callees are every
     implementation calling the handed value can run (`UnawaitedHandoff::complete`).
   - Every callee must be a generator function: a call that may run a plain or decorated
     implementation is no report. A report says so when a callee's name is bound to more than one
     definition (`MustExec::bound_twice` of its summary). Its tier is exact when no name is bound
     twice and the call is complete; the rule is heuristic until it is certified, which caps the
     tier.
14. **`EFF201`** (contract): a breadth-first search over the IR-free call graph from each scope
   function (and each module body a scope names as `<module>`,
   `DottedPattern::names_module_bodies`) finds the shortest chain to each forbidden effect,
   without entering `allowed-callees`, other scope functions, or bodies that do not run at the
   call (`async def` bodies not awaited, generators not iterated). Its nodes are `(function, Body | Result)`: a
   function returning a generator is entered as what iterating its result runs. The search
   reads no guards: an argument switching off a callee's branch does not stop it. At each node
   whose body runs, it also reads the function's accesses outside the project
   (`FileSummary::external_accesses`, per function, local and IR-free): the calls of external
   functions and classes (`Callee::External`), the reads of module attributes outside the
   project (an `AttrLoad`, a global or local load, or an `Import` whose value is the named object
   `Val::Ref(fqn)` of no project symbol: `sys.platform`, `from sys import platform`), and the
   import probes (an `Import` whose block's exception edge reaches an `except` clause naming
   `ImportError` or `ModuleNotFoundError`, through the clauses of each `try` and the re-raise to
   the enclosing handler). An access is kept only when a boundary forbids it
   (`Contracts::external_call`, `external_read`, `import_probes`): a `call-targets` or
   `read-targets` entry names it by its full dotted name (its first name not a glob), or
   `reads-environment` and the Layer 2 tag `environment` name it. A call of a function tagged
   `environment = "import"` records whether it imports a module of the project
   (`ExternalAccess::imports_project_module`, no read of the environment): outside a `try`
   taking an `ImportError`, every text its `name` argument may hold (as for `EFF103`'s
   statements, `summary/statements.rs`, a `str` method changing the case or the blanks giving
   none) names a project module the packages along the name lead the import to first
   (`ProjectDecls::holds_module`), or starts with a package and a dot under which the import
   finds only project modules (`ProjectDecls::holds_modules_starting_with`): the packages along
   the name lead it to the project's directories alone, and no package under it changes its
   `__path__`; a relative name is anchored at the texts of `package`, or at the module's own
   `__package__` or `__name__`. Where an import looks under a package
   (`ProjectDecls::search_under`) follows the packages along the name from the project's
   top-level names: a regular package of the project is found first, a directory without
   `__init__.py` is a namespace package with its parent's order, and a package's `__path__`
   changes it. What a module does to the `__path__` of its own package, or of one its imports
   name, is a declaration fact (`ModuleDecls::path_changes`, a `PathChange` per package):
   `Appended` for directories after the package's own (`append`, `extend`, `+=`, `__path__ +
   ds`, `pkgutil.extend_path`, `pkg_resources.declare_namespace`), `Replaced` for any other
   change or a use it does not follow (handing the list to a function other than a builtin
   that only reads it and `pkgutil`'s readers, returning or storing it elsewhere). In each
   function, the uses of the values holding a path (its loads and the values bound to it) are
   joined over the values each reaches through copies, phis, and `+=` (a `Closure`, so a value
   merging the paths of many packages is decided once); binding a value to a package's
   `__path__` leaves that path as it is and shares the list with any other package the value
   holds. A value bound to a path appends only when the value it adds to (`p + ds`,
   `extend_path(p, ...)`) holds that path on every way it comes, back through copies, phis, and
   `+=` (a second `Closure`). The walks of one function spend one budget; past it a package's
   path counts as replaced. `ProjectDecls`
   joins them per package with the packages heki could not read (`changed_paths`, sorted, so
   the packages under a name are one range). The project modules and packages it looks up are
   declaration reads (`DeclKeys::modules`, `DeclKeys::package_paths`; a change of a package's
   path changes the key of each package above it), so a module created, deleted, or changing a
   path re-summarizes the function.
   A module body's accesses include those of the class bodies it runs, as a function's do.
   Without a boundary forbidding one the map stays empty. The language server compares a
   file's accesses before and after an edit, without positions, and re-checks the files above
   a function whose accesses changed, as it does for its Layer 2 call sites.

### 2.5 Layer 3: on-demand analysis of unregistered dependencies (planned)
- **Boundary (strict)**: **never enter the standard library or packages registered in Layer 2**
  (A.7: avoids the `asyncio.StreamWriter.write` → `socket.send` misfire).
- **Limits**: at most 3 hops of exploration, 10,000 lines per module, 200 analyzed modules in
  total.
- **Disk cache key**: `(heki_version, effective_kb_digest, env_packages_digest, package_name,
  package_version, rel_path, file_hash)`.

### 2.6 Import facts and the import-time closure (`heki_effect::imports`, ADR-022)
`heki_effect::imports` is the one home of which module an import site loads; no rule resolves
an import itself.

- **Sites** (`FileSummary::imports`, per file, local and IR-free; extracted with
  `OptInFacts::imports` only, and never for a typing stub, whose imports never run): every
  `import a.b` and every name of `from m import x` (`ImportForm::Import`, `From`), every
  `from m import *` (`Star`; the IR keeps where each runs, `ModuleIr::star_imports`), and every
  call the catalog says loads a module by name (`ByName`: `importlib.import_module`,
  `__import__`; `loads_module`, 4.2), with the modules it names made absolute (a relative one
  from the module's name and package flag, none when it climbs past its top-level package; a
  by-name call's every text, as `EFF201`'s `imports_project_module` reads them, open when one is
  unknown or `__import__` is passed a `level` other than `0`). Each site carries its function
  (`<module>` for the module body and the class bodies it runs), whether an `ImportError`
  handler takes its failure (around the import, or around the class statement holding it), and
  whether it runs on every path to a normal return of its function, outside loops (never
  established in a class body).
- **Timing** (`imports::Timing`): `type-checking` under `if TYPE_CHECKING:` (the semantic
  model's guards); `function` in a function body; `lazy` for a lazy import (1.1) whose binding
  no read of the module body (or of a class body it runs) reaches, `body` when one may (the
  first such read loads the module there: the site is `unconditional` when a read only its
  binding reaches runs on every path, and guarded when an `ImportError` handler takes every
  such read). A read sees the bindings of its name (stores and star imports, which may bind any
  name) that reach it through the module body's blocks, an exception edge passing on every
  binding of the block it leaves (`walk::Closure` per name and block, one budget of
  `Budget::STEPS_PER_UNIT` per IR unit of the module body); a read heki cannot place, in a class
  body or past that budget, of a name bound more than once leaves the lazy imports of the name
  open. `body` for any other import of the module body or of a class body it runs. An import
  `__lazy_modules__` may make lazy has an open timing. A site's timing says where the import is
  written and its class, never that a branch is taken.
- **The graph** (`ImportGraph`, built per check from the summaries and the declarations, never
  kept in the index): what each site loads (`Target`). `import a.b.c` loads `a.b.c`, after its
  parent packages. `from p import n` loads `p.n` when the project has that module and
  `ProjectDecls::from_import` says the name is the submodule, `p` when it is the package's
  attribute or `n` is no module the package binds, and `p` with `p.n` open when the package binds
  `n` before the submodule is imported, or binds no `n` and the project has no module `p.n`, which
  Python then looks for outside the project's files (a compiled extension module)
  (`PackageOrSubmodule`); for a package outside the project, `p`, with `p.n` open. A module below a project package that the project does not have is `Missing`; one the
  project has but an import may find elsewhere first (`ProjectDecls::holds_module`), or a name the
  code does not fix, is open. The lookups are declaration reads (`DeclKeys::modules`).
- **Loads while it loads** (`ImportGraph::loads`): a module's `body` sites that run on every
  path through the body (`unconditional`) and that no `ImportError` handler guards
  (`ImportSite::loads_while_its_module_loads`) load what they certainly load
  (`Target::certain`: the module, or the package of a `from` import) with its parent packages;
  a site on one branch (`if sys.platform == "win32": import winreg`), in a class body, or whose
  timing or module is open adds nothing, so what follows is what loading a module loads at
  least.
  `ImportGraph::loaded_while_loading` walks it from one module (`walk::reach`), and
  `ImportGraph::import_time` (`walk::Closure`, the members of an import cycle sharing one fact)
  gives every module the modules outside the project it loads that way. Both spend one budget of
  `Budget::STEPS_PER_UNIT` per module and site (`Budget::of_graph`), asked in the order of the
  modules, which `heki check` and the language server build alike; past it the answer is open.
- **Contract walks** (`heki_rules::imp_rules`, `import-owners` with `reach`): from an importer,
  breadth first over the sites of the classes the contract counts (`walk::reach_breadth_first`),
  each certainly loaded module and its parent packages (`ImportGraph::parents`, which names the
  parents of every site's module, whatever its timing) a step, keeping the step into each module
  from the nearest one, so the chain reported is a shortest one. The packages holding the
  importer load before it and are no step. The importers of one contract walk in the order of
  the modules and share one `Budget::of_graph`; past it they answer open and report nothing. A
  check without such a contract resolves only the checked modules' sites
  (`ImportGraph::resolving`), so the language server records only the declarations they read.
- **Cycle groups** (`ImportGraph::cycle_groups`, for `IMP101`): a site of the classes asked
  steps into the module it certainly loads and into the parent packages that load first, except
  the packages holding its own module, which loaded before it ran (`ImportGraph::hops`,
  `ImportGraph::holds`); an import naming such a package itself is a step. Per module, one hop
  into each module, chosen by when its site runs and then by source order. The groups are the
  strongly connected components of two or more modules (`scc::tarjan_scc`), and each gets a
  shortest cycle through its smallest module, breadth first over the hops in the order of the
  modules they enter (`walk::reach_breadth_first`). Both are linear in the modules and sites and
  unmetered: Tarjan takes each hop once, and each search walks the modules of its own group,
  which no other group shares (one step per module, owner `import_cycles`).
  The language server re-checks the files whose groups close a cycle, before and after an
  import changes (`Workspace::note_import_cycles`).
- **Early Cutoff**: `IncrementalUpdate::imports_changed` tells whether a file's sites changed,
  without positions (`ModuleImports::same_facts`), or appeared or disappeared with the file.
- **Kinds of code** (`heki_effect::code_kind`): a test module (under `tests/` or `test/`,
  `test_*.py`, `*_test.py`, `conftest.py`) is development code; in the project's own src
  layout, the code below a source root is the package's and the code outside every root
  (`scripts/`, `tools/`) is development code; otherwise a module is the package's when its
  top-level package is named like `[project] name` (normalized) or by the build backend's
  settings (`heki_env::Declared::packages`), and open when not.
- **Declarations** (`heki_env::Declared`, read with `requires-python`): the runtime
  dependencies (`[project] dependencies`, Poetry's non-optional ones), the extras, the
  development groups (PEP 735 `[dependency-groups]` with `include-group`, `[tool.uv]
  dev-dependencies`, Poetry's groups and `dev-dependencies`, `[tool.pdm.dev-dependencies]`), the
  `dynamic` fields, the entry points, and the packages the build backend names, distribution
  names normalized as PEP 503 does.
- **Checked against CPython** (`tests/import_verify/verify_imports.py`, CPython 3.11 to 3.15):
  each fixture module is imported first in a fresh interpreter that records the imports the
  module bodies run, and the `body` edges are compared both ways; an `ast` reader checks the
  function, `TYPE_CHECKING`, and `lazy` classes.
- `heki inspect --imports MODULE` shows the sites of the modules a name or glob names, with what
  each loads, and what loading the module loads.

---

## 3. Soundness / Alias lane (`heki_soundness`): family 3

### 3.1 Abstract domain with provenance (several `alloc_id`s, derived references)
```text
const MAX_JOIN_ALLOC_IDS: usize = 4;

enum ExactTypeVal {
    Exact { ty: BuiltinKind, origin: Span },
    Unknown { reason: String },
}

enum UniqueState {
    Unique { alloc_ids: SmallVec<[AllocId; 2]> },   // more than 4 after a CFG merge: Shared
    Shared { witness: EscapeWitness },              // PassedToUnknownCall, StoredInContainerOrAttr,
                                                    // DerivedRefEscapedAt, JoinAllocOverflow, ParameterInput,
                                                    // CapturedByClosure, ExternalValue, ReturnedOrRaised
}

struct FrameState {
    var_type:     Map<VarId, ExactTypeVal>,
    var_unique:   Map<VarId, UniqueState>,
    alias_sets:   Map<AllocId, Vec<VarId>>,
    derived_refs: Map<AllocId, Vec<VarId>>,          // MakeIter / MakeGenExpr / retained_in_return
    derives_from: Map<VarId, SmallVec<[AllocId; 2]>>,
    points_to:    Map<VarId, SmallVec<[AllocId; 2]>>,// kept when the variable becomes Shared
    alloc_elems:  Map<AllocId, ElemKinds>,           // element kinds of list / tuple / set allocations
    points_to_incomplete: Set<VarId>,                // may also hold an object no site names
}
```

### 3.2 Transfer rules, derived references, merges, `ValueLoopInvariant`, and the A2 `SoundnessSummary`
- **Strong update**: rebinding and `Delete(x)` remove `x` from every `alias_sets` /
  `derived_refs` entry.
- **Derived references and transitive escape**: `MakeIter(r, x)` / `MakeGenExpr(r, x, ...)` /
  Layer 1 `retained_in_return` add `r` to the `derived_refs` of every `alloc_id` of `x` (chains
  are inherited transitively). When `r` escapes, every alias of every original `alloc_id` becomes
  `Shared { witness: DerivedRefEscapedAt(span) }`.
- **CFG merges**: `Shared` on either side gives `Shared`; two `Unique` values give the union of
  their `alloc_ids` (kept when at most 4, otherwise `Shared { witness: JoinAllocOverflow }`).
- **Element kinds** (`alloc_elems`, used by `HashConsistentProbe`, `ZeroTripExceptionSafe`,
  and loop-element types) hold only where they describe every object a variable may hold:
  - a parameter, an unknown result, or a container element has no allocation site, so a
    variable that may hold one is in `points_to_incomplete` (unioned at merges) and has no
    element kinds;
  - an escape (`mark_escaped`) makes the escaped objects' kinds unknown: the code that
    received them may replace any element whenever it runs;
  - a site in a loop that runs again while a variable or iterator still refers to its earlier
    object joins the new kinds into the old ones instead of replacing them;
  - `next(it)` has the element kinds of the object `it` was made from by `iter()` (a `for`
    loop, a comprehension) only, not of what an iterator retains (`map(f, xs)`, a generator
    expression);
  - `seq[i]` reads one element only when `i` is an exact `int` / `bool` (`seq[s]` with
    `s = slice(...)` is a new sequence);
  - a call with `*` arguments does not use its Layer 1 entry (the positions do not line up).
- **Function objects**: creating one escapes every operand of `MakeClosure` (captured
  variables, and default values when the IR passes them).
- **Formatting**: `Convert` or `Format` of a value that is an exact builtin on every path runs
  builtin code only, which hands the value's items to their `__repr__` but never the value
  itself, and gives a new exact `str`; of any other value it escapes the value and the
  specification, whose class's `__str__` / `__repr__` / `__format__` may keep them. `BuildString` gives a new exact
  `str`. A rewrite that changes a formatted value's type changes its text (`OPT120` /
  `OPT121` block on it).
- **`ValueLoopInvariant(v, loop)`**: besides `BindingLoopInvariant`, the value is an immutable
  builtin (a `tuple` whose elements are hash-consistent builtins too), or a mutable builtin that
  stays `Unique` for the whole loop and whose `alloc_id` (aliases and derived references included)
  is never changed inside the loop (against S27–S28).
- **Conservative `SoundnessSummary` for A2 (planned)**: computed inside `heki_soundness`, without
  depending on `heki_effect`, only for statically bound functions (defined once at module level,
  undecorated, not mocked). Until then every call into project code counts as unknown.

### 3.3 Proof obligations per family-3 rule (safety and profitability kept apart)
Every candidate also needs `A1HazardFree` (no frame introspection in the function) and
`AnalysisConverged` (the abstract interpretation reached a fixpoint).

| Rule | Profitability | Safety obligations (behavior preservation) | Profitability condition | Main counterexamples |
|---|---|---|---|---|
| **OPT101** | `unconditional` | `Unique(x)`, `ExclusiveAccessAtWrite(x)` (every alias and derived reference dead), `ExactType(x / rhs, list)`, `ExceptionStateSafe(x)`, `EvalOrderPreserved` | `LoopCarried(x)` | S1–S5, S14, S23–S25 |
| **OPT102** | `unconditional` | `Unique(target)`, `ExclusiveAccessAtWrite(target)`, `ExactType(target / rhs, set \| dict)`, **for dict unpacking the accumulated `d` comes first (`{**d, ...}`): `DictUnpackLeading`**, `ExceptionStateSafe` (also `(update)`: an update inserting several keys is not observed half done by a handler), `EvalOrderPreserved` (also `(update)`: entries gathered into one `update` have builtin keys and later entries that run no code), **for a set `SetOrderUnobserved`: every value the write's result reaches through copies, phis, and set operators on exact sets is only tested (`x in s`, `len`, `bool`, `not`, a branch, `==` / `!=` with a builtin value, `del`, `raise`), never iterated, converted, formatted, returned, stored, yielded, or passed to other code** | `LoopCarried(target)` | S26, S26b, **datamodel-code-generator `_target_binding.py:465` (`{**other, **found}` accumulates on the right and reverses the override order)**, a set of `int`s iterated after the loop (`s \|= {x}` orders it differently from `s = s \| {x}`) |
| **OPT103** | `size-dependent` | `Unique(acc)`, `ExclusiveAccessAtWrite(acc)`, `ExactType(acc / rhs)`, `NoIntermediateReads`, `ExceptionStateSafe` | `LoopCarried(acc)`, `NotConstSmall` (local `str +=` excluded); for `obj.attr +=`, every assignment of the attribute (the class's methods for `self`) stores a `str` / `bytes` / `tuple` literal of the right operand's kind | A.8 (slower for small loops), `self.s +=`, `__radd__` on the right, `self.values = []` then `self.values += (i,)` |
| **OPT110** | `size-dependent` | `ValueLoopInvariant(lst, loop)`, `ExactType(lst, list \| tuple)`, hash-consistent builtin elements (`HashConsistentProbe`), `ZeroTripExceptionSafe`; the edit needs a `for` statement (not a comprehension clause) and a name no other hoist of the function binds | `NotConstSmall(lst)`, not a `ConstSmall` / retry loop | S9a, S9b, S22, S27 |
| **OPT111** | `size-dependent` | `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, list)`, hash-consistent builtin elements, only the appends the set mirrors (`OnlyPairedAppends`) | `NotConstSmall` | `seen.remove`, unhashable elements |
| **OPT120** / **OPT121** | `size-dependent` | `Unique(q)` (no escape, not returned), `ExactType(q, list)`, only operations `deque` supports equally (`DequeEquivalentOps`: no `list(*a)` / `list(**kw)`, no iterator live at a change or consumed by it, iterators used only for their elements, `q = q[1:]` exactly, no other object bound to a variable it reaches) | `NotConstSmall`, **no index `q[i]` other than the ends (`0`, `-1`)** | S12, `sort()`, $O(n)$ middle indexes, `q.extend(reversed(q))`, `type(iter(q))` |
| **OPT130** | `size-dependent` | Every argument `ValueLoopInvariant`, `SideEffectFree` (no method arguments), `ZeroTripExceptionSafe` (no `*` arguments), the built container used by the membership test only (`ValueLoopInvariant(call)`) | `NotConstSmall` (100 times slower when the loop runs zero times: A.8), not a `ConstSmall` / retry loop, not `tuple(t)` of a tuple | S22, S28, A.8 |

Open premises (`CheckExtras::open_premises`, as for `EFF111` / `EFF113` in section 2.4): a
heuristic report of an `OPT` rewrite records "Rewrite proven safe" once for each obligation of
its verdict that is not established, with the obligation's kind as the cause
(`heki_rules::premises::OBLIGATION_CAUSES`, one row per `ObligationKind`) and its description
(`Kind(subject): reason`) as the detail. The other premises of the row hold for every report: a
site is found only in a loop and, for an accumulation, on a value carried around it
(`find_sites`), and a candidate is reported only when its construction establishes the builtin
type (`OptCandidate::is_reportable`). The exact tier of a size-dependent rewrite also asks for a
condition that is no premise, "Trip count open" (`heki_rules::premises::Condition`; Safety ×
Profitability, ADR-007 and ADR-011: the claim holds either way, but the change pays off only
over a loop the data sizes), which the Advisory lane decides for the loop around the site
(`OptCandidate::trip_loop`, in `heki_rules::opt_rules::TripCounts::open_record`): the code does
not fix or cap its trip count (`FnIndex::fixed_trips`, computed with the candidate), and its
items are the data's as `EFF103`'s "Origin" establishes them (`DataOrigins::of_loop_items`: a
parameter or a part of one, `*args` included, or the rows, lines, or entries of a call the
catalog sizes, through `ItemOrigins::of_loop` and `data_origin`; a walk budget per function). A
loop over a receiver's attribute, a project function's result, `range` of a number that is no
constant or length, an item of a call's result (`type(x).__mro__`), or a `while` loop runs as
many times as the program has parts, however large the input. A proven report over such a loop
leaves no premise of its row open; it records the condition with its cause in the same list as
open premises (`heki_diagnostics::OpenRecord::Condition`, axis profitability): `trip-count-fixed`
with the bound as its detail, or the cause of "Origin" that rejected the items, under the same id
and closer (`premises::trip_count_cause`). Every heuristic report names why it is not exact; the
premise rows, the claims, and the oracle's strips do not change.

`OPT113`'s exact tier is one decision, `JoinPremises::steps`, which tells each step that does
not hold to its caller in the order it takes them (the same collection, the same keys, both
loops through inputs, both trip counts open, idle on a mismatch, the inner loop advanced
without code): the decision stops at the first, so it spends the function's shared walk budget
as before, and an explanation goes on with a budget the function's explanations share (one more
budget proportional to the function), so the decisions of the other joins are unchanged. `idle_on_mismatch` tells a key load that may run code ("Equality of keys":
of an item whose type is unknown, or a load that is no plain attribute of a known class or no
item of a builtin container) apart from any other work on a mismatch ("Idle on a mismatch"), and
`steps_run_no_code` an inner iterable of unknown type (an annotation closes it, unless a project
subclass of a builtin container defines `__iter__`) apart from one that may run code. A trip
count the code fixes or caps records "Trip count open" at the loop, as for the rewrites. Its
loops' items need no condition of their own: "Steps through inputs" asks for parameters or parts
of them, which "Origin" accepts too, so a join over a loop the data does not size is heuristic
for that premise.

---

## 4. Knowledge bases, the Ratchet baseline, and profile dumps

### 4.1 Layer 1 (`kb/soundness/*.toml`, Soundness lane only)
No advisory `effects` field at all: an entry has only `symbol`, `return_exact_type`,
`param_effects` (`read_only`, `mutated`, `retained_in_return`, `escaped`; for methods index 0 is
`self`), and `verified_max_python`. Entries whose `verified_max_python` is older than the target
version become `Unknown` when the knowledge base is projected at start-up. `#[serde(
deny_unknown_fields)]` rejects a Layer 2 field written into a Layer 1 file and the other way
round; `tests/sentinel/verify_soundness_kb.py` checks every claim on real CPython.

### 4.2 Layer 2 (`kb/ecosystem/*.toml`: public standard-library APIs and major packages)
The single place for every execution-effect origin. An entry has `symbol`, `effects`
(`blocking_net`, `blocking_db`, `blocking_subprocess`, `blocking_sync`, `sleep`, `blocking_fs`,
`unpooled_client_alloc`, `db_query`, `quadratic_realloc`), `async_safe` (`asyncio.StreamWriter.write`),
`offloads_blocking_arg` (`asyncio.to_thread`; with `0`, also an offloading decorator such as
`asgiref.sync.sync_to_async`), `sync_facade_arg` (`AsyncSession.run_sync`: the callable runs on
the caller's thread with a synchronous facade over an async connection), `excluded` (`logging.Logger.*`), `copy_sanitizer` (`"deep"`:
`copy.deepcopy`, `BaseModel.__deepcopy__`; `"shallow"`: `copy.copy`, `BaseModel.model_copy`, whose
`deep=True` copies deeply unless `update=` hands it values by reference: `MUT202`),
`stores_on_instance` (a class's `__setattr__` / `__delattr__` that stores the value, possibly a
validated conversion of it, or deletes the attribute on the instance itself, or raises, handing
the value to no other object, in a class that installs no descriptor for it and defines no
`__getattribute__`, so a read finds the stored value: `pydantic.BaseModel.__setattr__`),
`builds_receiver` (the call builds a new instance of its receiver's class without its `__init__`,
the fields set from its keywords or from the mapping at a position or keyword, only from a dict
where `only_from_dict` says so: `pydantic.BaseModel.model_construct`, `model_validate`,
`model_copy`; a call contract sees a construction of the class), `keeps_hashable_defaults` (an
instance built without a field starts from the field's default itself when it is hashable, and
from a deep copy of an unhashable one, then holds what validating the default returns where the
field validates it: pydantic 2's `BaseModel`), `return_type` (factories such as
`sqlite3.connect`), `sync_lock_or_tx` (`threading.Lock`, `Session.begin`: `EFF106`),
`non_reentrant_lock` (`threading.Lock`, `asyncio.Lock`: `EFF109`), `one_at_a_time`
(`asyncio.StreamReader.readline`, `AsyncSession.execute`: `EFF110`), `writes_in_order` (the
argument a call writes or serializes in the order of its elements, with the keyword that sorts
mapping keys first and whether it sorts by default: `json.dumps`, `yaml.safe_dump`,
`pprint.pprint`, `pathlib.Path.write_text`, `print`: `EFF112`),
`waits_until_done` / `done_when_awaited` (futures), `result_of` (the call hands back, awaited or
by the future's `result()`, what calling the callable at position `arg` returns, with the
positional arguments after it and, with `keywords`, the call's keywords: `asyncio.to_thread`,
`loop.run_in_executor`, `anyio.to_thread.run_sync`, `executor.submit`; section 2.1),
`bounded_by` / `waits_while_full` (queues:
section 2.2), `environment` (`"call"`: calling the function reads the environment the program runs
in, `os.getenv`, `platform.system`, `shutil.which`; `"read"`: reading the module attribute does,
`os.environ`, `sys.platform`, `sys.version_info`; `"import"`: calling the function imports or
finds the module its argument `name` names, a relative name anchored at its argument
`package`, which reads the environment unless the module is the project's,
`importlib.import_module`, `importlib.util.find_spec`: what `EFF201`'s `reads-environment`
forbids; such an entry has no other fact but `loads_module`), `loads_module` (calling the function
imports the module its arguments name, an import site of the caller (2.6): the argument holding
the name, the one a relative name is anchored at, and the one giving how many packages up a
relative name starts, which must be absent or `0`; `importlib.import_module`, `__import__`;
`importlib.util.find_spec` only finds one), and
`opaque_to_layer3` (reserved for Layer 3), and `cases`: what the call does instead when one
argument (by position, the receiver not counted, or by keyword) is a given literal (an
argument is the literal it is, or the one a module constant bound once holds:
`TypeCtx::global_literal`, `ModuleDecls::str_constants` and `literal_constants`, the latter
read from `heki_ir::ModuleConstants` (the integers it computes from other constants, its
booleans, and its floats), the one view of a module name bound once that the IR's facts read
too, with `heki_ir::GlobalStores` the one count of its stores and deletions, also imported
unless the importing module binds the name again, `ModuleDecls::fixed_import`), a string
literal with a given prefix, equal to one of given strings (`one_of`), containing one
(`contains`), an IPv4 or IPv6 address (`numeric_address`), or not empty, a value established to
name a file (`names_file`: a literal or module constant without an in-memory form, or a path
built from such parts and `__file__`), an instance of a given class, or an integer that sets
a named flag, alone or as an operand of `|` (`pd.read_csv` of a `https://` URL is
`blocking_net`, of an `io.StringIO` nothing; `socket.getaddrinfo(..., flags=AI_NUMERICHOST)`
resolves nothing); the first case that holds sets
the call's `effects`, `return_type`, and `emits`, and an argument that is not passed, or may be passed
through `*` / `**` unpacking, selects none. A boolean flag case (`equals = false`) whose argument
is a parameter of the calling function (`shutdown(wait=wait)`) becomes a guard: the effects it
switches off need the parameter to be true, which the callers decide (`close(wait=False)`). A file lists its `opaque_packages`. Projects add or
override entries with `[[tool.heki.ecosystem]]` (`symbol`, `effects`, `async-safe`,
`offloads-blocking-arg`, `excluded`, `copy-sanitizer`, `return-type`, `sync-lock-or-tx`,
`non-reentrant-lock`, `one-at-a-time`, `writes-in-order`, and `attribute-type`, which adds an
`[[attributes]]` row and takes no other key): an
override sets the keys it lists and keeps the built-in entry's others. A `[[tool.heki.ecosystem]]`
entry naming a project function applies to its summary (the built-in catalog never does: a
package's own source is followed through its bodies): its `effects` are direct effects (chain: the function itself),
`offloads_blocking_arg` a parameter it offloads, `excluded` / `async_safe` hide every / the
blocking effects of the final summary from its callers (`EffectSummary::declared_hidden`), and
its `return_type` types the call over the annotation. An `async_safe` entry
keeps only its loop-sensitive effects when the catalog is loaded (built-in entries carry no
blocking effect at all).

Three more keys state when a call waits, which an exact claim needs (ADR-018):
- **Wait kind.** A blocking effect waits for input or output the call performs (`blocking_net`,
  `blocking_db`, `blocking_subprocess`, `sleep`, `blocking_fs`), or for another thread
  (`blocking_sync`), which may already have done its part: that is state at run time
  (`ExecEffectKind::wait_kind`).
- **Arguments that end a wait at once.** Every wait entry has a case for each argument value with
  which it returns at once: `time.sleep(0)`, `Popen.wait(timeout=0)`, a zero timeout or
  `blocking=False` of the thread waits, a numeric host or `localhost` for `socket.getaddrinfo`
  and `socket.gethostbyname`, a `data:` URL for `urlopen`. An argument heki cannot evaluate takes
  the entry's own effects: it counts as a value for which the call waits (R6).
- **`emits`** on the database entries: `always` (the default), `on-identity-map-miss`
  (`Session.get`), `if-pending` (`Session.flush`, `Session.commit`, a driver's `commit()` /
  `rollback()` without an open transaction does nothing), `unless-cached` (Django's
  `QuerySet.count()` / `exists()` / `first()` / `last()` on an evaluated query set read its
  cache; an exhausted pymongo or motor cursor returns nothing without a query), `unless-connected` (a pool checkout, Django's `connection.cursor()`),
  `unless-configured-otherwise` (any effect, on any entry: the library can be configured so the
  call does not do it, as Celery's `task_always_eager` or an in-memory transport; no effect of
  the call is certain). Only `always`
  establishes that the call sends or waits on every call.
- **`items`** on a call whose result's items the environment decides the number of: `rows`
  (`Cursor.fetchall`, a DB-API `execute` whose cursor is iterated, `Session.execute`, pymongo's
  `find`), `lines` (`open`), `entries` (`os.listdir`, `os.scandir`, `os.walk`), `text`
  (`Path.read_text`, whose `splitlines()` pieces are lines). A loop over such items, or over a
  parameter, is a loop over data (`EFF103`).
- **`exit_suppresses`** on a class, or a factory whose result a `with` statement manages:
  `false` when `__exit__` / `__aexit__` returns a false value on every path (files, locks, HTTP
  sessions and pools, database connections), `true` when it can suppress the body's exception
  (`contextlib.suppress`). A manager the catalog does not describe may suppress.

A separate table, `[[invokes]]`, says what a call runs of the values it is handed (R3; read by
`heki_effect::invokes`, so that no other fact sees an entry there): per argument (by position,
`rest` for every position from it on, by keyword, `"*"` for any keyword, or the receiver), the
special methods it runs on the value's class (`methods`, looked up on the metaclass for a class
object; `"*"` for any attribute, read by a name the call is handed), on the classes of the
value's items (`item_methods`), and whether it calls the value (`calls`). An argument an entry
does not list runs nothing there. A `lazy` entry returns an iterator that runs the same code
again while it is iterated (`enumerate`, `map`, `iter`). A value runs code of the project there
only when its class may define the method in the project: a project class, a builtin class a
project subclass replaces it in (R2), or a class heki does not know. Two rules keep the entries
short: a key a dict or set looks up is compared with the keys it holds only on a hash collision,
which an entry leaves out, and a builtin dict or set hands on its keys with the hashes it stored
for them. A call of a catalogued library without such an entry runs code of the project only
when it is handed a value not established as an instance of a library class that is not
callable; `eval`, `exec`, `__import__`, `importlib.import_module`, and `pickle.loads` always may.

A fourth table, `[[attributes]]` (`heki_kb::AttributeType`), gives the class of what reading an
attribute on an instance of a library class gives (`symbol = "openai.OpenAI.chat"`,
`type = "openai.resources.chat.Chat"`), read as section 2.1 says. A row states that in every
release its distribution's verified range admits, the value read has the catalogued members of
that class; it says nothing of its identity or of what the read runs, so a plain attribute, a
property, and a cached property are alike to it. It is a table of its own because an attribute is
no method a class takes from its base (`external_defines`). The synchronous and asynchronous
clients of a library have rows of their own at every step, and a row whose meaning changes
across the verified releases under one name is left out. `[[distributions]]`
(`heki_kb::Distribution`) names, per distribution (its PyPI name), the import packages its rows
cover and the PEP 440 range of releases they were checked against (`verified`), which
`tests/sentinel/verify_ecosystem_kb.py` installs and checks the rows on. The analysis never
reads a project's versions: its output does not depend on the installed environment. The rows
type the resources of the LLM SDKs: openai's clients (`OpenAI`, `AzureOpenAI`, and their async
twins) to `chat.completions`, `responses`, and `embeddings`, anthropic's to `messages`, and
google-genai's `Client` to `models` and to its async client `aio`; the synchronous requests
(`create`, `generate_content`, `embed_content`) wait on the network, the asynchronous ones are
`async_safe`, and `with_options()` / `copy()` return `"Self"`.

`[[provides]]` (`heki_kb::Provides`) lists the modules a distribution installs under names
other than its own (`PyYAML`: `yaml`, `_yaml`; `Pillow`: `PIL`; `protobuf`: `google.protobuf`),
which `EffectiveEcosystemKb::distributions_providing` looks up by a module or a package holding
it; a dependency's other modules are those whose top-level name, normalized like a distribution
name, is its own.

A third table, `[[cached_properties]]` (`heki_kb::CachedPropertyDecorator`), lists the
cached-property decorators known to keep the getter's value in the instance `__dict__` under the
property's name: `functools.cached_property`, Django's, Werkzeug's, propcache's, and kombu's. Each
entry names the versions whose source was read, why the value is kept there, how `del obj.<name>`
and `obj.__dict__.pop(<name>)` act on it, and, for Werkzeug, the slot an instance without a
`__dict__` keeps it in. `KeptFacts::listed_cache` records that a getter's decorator, resolved
through the module's imports (`ModuleDecls::decorator_symbol`), is listed; only such a getter makes
a cached property an exact claim's (R3), and any other `cached_property` is one by its name.

A fourth table, `[[calls_unawaited]]` (`heki_kb::UnawaitedCall`, `EFF115`, `EFF116`), names the
callable argument a call calls as a plain function without awaiting what it returns (by position,
the receiver not counted, or by keyword), whether it tests the result for truth, and whether it
hands the result back to its caller (`hands_back`: an executor's `submit` and `map`,
`weakref.finalize`): `threading.Thread`'s `target`, `atexit.register`, `signal.signal`, `filter`.
Every `offloads_blocking_arg` API counts as one too, and hands the result back. Each listed call also has an entry; the table
stands beside the entries so that the entries without such an argument, nearly all, keep no word
for it.

`[[declared_fields]]` (`heki_kb::DeclaredFields`) says where a library keeps the fields a class
declares, the names the bodies of the class and its bases declare as fields: the attributes of
the classes deriving from its `bases` or built by its class `decorators` (pydantic's
`model_fields`, `__pydantic_fields__`, and `__fields__`, also pydantic 1's; a dataclass's
`__dataclass_fields__`; an attrs class's `__attrs_attrs__`), read on the class or an instance, and
the `functions` returning them for the class, or the instance, passed first
(`dataclasses.fields`, `attrs.fields`, `attrs.fields_dict`), each holding a field's name as a key
of a mapping or as the `name` of each object. `ProjectDecls::declared_fields_attr` finds an
attribute's row as `catalog_attribute` finds an `[[attributes]]` row (every class the value may
be under R2 finds the same one first along its MRO, a decorated project class counting as one
that binds it), and `stores::key_name` reads a key a loop takes from them as an attribute the
class declares (section 2.1). Each row names the versions it was checked against.

The entries follow the public API of each package, with three conventions:
- An async API is `async_safe` and carries no blocking effect: it is often called without a
  direct `await` (`async with`, `asyncio.gather`, `asyncio.wait_for`), where the `is_awaited`
  filter does not apply. Its loop-sensitive effects (`db_query`, `unpooled_client_alloc`) stay.
- A constructor or factory that does no I/O until first use (`redis.Redis`, `pymongo.MongoClient`,
  `sqlalchemy.create_engine`) carries only `unpooled_client_alloc`; a call that only builds an
  object (`Redis.pipeline`, a lazy `QuerySet.filter`, `Session.query` and the `Query` builder
  methods) carries only a `return_type`. Iterating an object runs its class's `__iter__` entry
  (`sqlalchemy.orm.Query.__iter__`): a `for` loop, a comprehension, a generator expression, or a
  consuming call (`list(query)`, `items.extend(query)`, the `consumption` table) over a value of
  that class is a call of it at the iterable, and an `async for` runs its `__aiter__` entry. A
  call returning a lazy iterator (`pymongo`'s `find()`, the generator function `os.walk`, whose
  result class is named after the call: `os.walk()`) carries only that `return_type`: its I/O
  runs where the iterator is iterated, on a worker thread when it is handed to one.
- `db_query` is one statement sent to the database and waited for: reading the rows of an
  executed statement (`sqlite3.Cursor.fetchone`) is not one, nor is a driver's `commit()`; an ORM
  `Session.commit()` / `flush()` sends the pending writes and is. A bulk API (`bulk_create`,
  `in_bulk`, `insert_many`, `bulk_write`) handles many rows in one statement and is called per
  batch by design: it blocks, but it is no `db_query`.

Lookups are exact, without inheritance or aliases (an entry names the class that a `return_type`,
an annotation, or a constructor call produces), and the `heki_kb` unit tests pin every entry.

### 4.3 Line-number-independent Ratchet baseline (`heki_diagnostics`)
- **Key**: `BaselineKey = (relative_posix_path, qualname, rule_id, normalized_target)` → `count`.
  Without line numbers, unrelated edits and reformatting do not break the baseline.
- **Rules (shrink only)**:
  - a key whose current count **exceeds** the baseline **fails** (a new violation);
  - a key whose current count is **below** the baseline (or has disappeared) prints a note asking
    to shrink the file with `heki check --update-baseline`.

### 4.4 Optional profile dumps (`heki_profile`)
- **Offline reading only (the program is never run or touched)**: collapsed stacks (CPython 3.15
  Tachyon `--collapsed`: `tid:N;file.py:func:line;... count`; py-spy: `func (file.py:line);...
  count`) and Tachyon `--gecko` JSON are read into their distinct stacks and sample weights
  (gecko samples summed per stack first; every table reference checked; a total that overflows
  is a malformed dump); `heki check --profile` uses it to apply proven size-dependent rewrites in
  hot functions.
- **Attribution to project functions**: each frame counts for the one project function its name
  (on a dotted boundary), file (on a path boundary), and line (inside the definition) can denote;
  a frame several functions fit counts for none, so an ambiguous profile never promotes a fix.
  A function's share is the weight of the stacks it is on, divided by the total weight.
- **Constraints checked on real tools**: Tachyon on CPython 3.15 cannot combine `--opcodes` with
  `--jsonl` (`--gecko` / `--binary` are needed), and macOS needs `sudo` to read another process's
  memory. Profile integration is therefore never required, only an optional add-on for tuning.

---

## 5. Stage 0-A measurement results (completed 2026-11-19)

339 repositories (118 mature open-source repositories with 12.42 million lines and 221 new GitHub
repositories of 2026 with 5.68 million lines; 56,629 files, 18,108,143 lines in total) were
scanned with 8 workers (62.8 s), and all 171 samples of the 8 rules were reviewed by hand.
These figures describe that analysis. Each rule's certainty and its current shape-right and
real-harm rates are in [docs/precision.md](precision.md).

| Rule | Hit rate by domain | Indirect share | Conclusion of the 171-sample review (analysis of 2026-11-19) | Decision in Baseline v4.0 |
|---|---|---|---|---|
| **EFF101** | Web/async **37.6%** (35/93) / DB 29.1% | **82.3%** (436/530) | **68% real harm** (including the real `time.sleep` in redis-py `cluster.py:1613`, reached in 4–5 hops) | **Core** (awaited calls and `Popen` excluded) |
| **EFF102** | Web/async **44.1%** (41/93) / DB **48.5%** (50/103) | **90–93%** | **About 75–80% real harm** after the `for.iter` fix (NeMo, OmniAgent, luigi, ...) | **Core** (`for.iter` and `DaemonPolling` handled) |
| **EFF103** | DB **47.6%** (49/103) / web 31.2% | **74.2%** (630/849) | Many N+1 queries; DDL, migrations, and ORM internals cause noise | **Core** (`for.iter`, DDL, ORM internals excluded) |
| **DATA101** | Data/ML **19.5%** (24/123) | — | All 60% false positives of a naive AST check were non-`LoopCarried` padding; with `LoopCarried` **about 90% of reports were right about the shape** in that review | **Core** (SSA `LoopCarried` required) |
| **MUT101** | Mature **16.1%** (19/118) / new 0.9% | — | Hit a real bug in datamodel-code-generator 0.86.0, and in mkdocs, wagtail, textual, ...; the about 64% noise of the naive version came from 3 causes | **Adopted in its strict form** (two-phase initialization, memo dicts, disjoint dict keys excluded) |
| **MUT102a/b** | Mature 4.2% / new 0.5–1.4% | — | Sparse and mostly deliberate idioms (zero-copy `Sequence`, `frozen` accumulating lists) | **Rejected by measurement** (datamodel-code-generator's **`MUT201` / `MUT202`** adopted instead) |
| **DATA102** | Data/ML 12.2% / mature 2.5% | — | A single-expression syntax check that duplicates Ruff / pandas-vet | **Rejected** |

*(The Wave 2 candidates were measured the same way: `MUT107`, `OPT112`, `OPT113`, `EFF106`,
`MUT106`, and `EFF107` are implemented; `MUT104` for `ALL_CAPS` constants, `EFF108`, and `MUT105`
were rejected; `OPT131` still waits for its measurement.)*
