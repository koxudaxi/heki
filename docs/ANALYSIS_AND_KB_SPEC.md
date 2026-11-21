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
    Before 3.12 a comprehension is a function of its own.
  - **3.14+ PEP 649**: annotations without `from __future__ import annotations` are evaluated
    lazily in `__annotate__` scopes.
  - **3.15+ PEP 810 (checked on CPython 3.15.0)**: `lazy import m` / `lazy from m import x`
    (`Import(is_lazy=1)` / `ImportFrom(is_lazy=1)`) are recognized and modeled as bindings
    materialized when the name is first evaluated, not when the module loads.
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
share a summary with a module-level `Worker`.

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
| Attributes and items | `AttrLoad`, `AttrStore`, `AttrDelete`, `SubscriptLoad`, `SubscriptStore`, `SubscriptDelete` | Stores make the stored value `Shared`; `AttrDelete` recognizes `@cached_property` invalidation (`del self.prop`) and `MUT201` writes; subscripts keep constant keys and slice bounds; the slice object itself is the tuple `(lower, upper, step)` of its bounds (`None` where one is left out), since `__getitem__` may keep it; a template string builds the tuple of its fields' values (`Template.values`) and is otherwise opaque |
| Calls | `Call(callee, args, kwargs, is_awaited)`, `MethodCall(recv, method, args, kwargs, is_awaited)` | `is_awaited` separates awaited calls from synchronous ones (a coroutine awaited after it is made runs at its `await`, which the effect summary follows); `star_from` is the index of the first `*` argument, so arguments bind to parameters exactly up to it |
| ContainerIteration and generators | `MakeIter(iterable)`, `IterNext(iter)`, `MakeGenExpr(outer_iter, captured)`, `MakeClosure(captured)` | `MakeIter` and `MakeGenExpr` bind a **derived reference** of `iterable` / `outer_iter` to an explicit SSA variable; `MakeClosure` holds every value the function or class object keeps: its cells (free variables of nested scopes at any depth, the iteration variables of inlined comprehensions, the names read by lazily evaluated annotation and type-parameter scopes), its defaults, eagerly evaluated annotations, and a class's bases and keywords (a `type` alias is a `MakeClosure` without a lowered body) |
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
  reaches again, as to a join, so its walk around such a cycle ends.
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
    tests on the single-predecessor way to each operand. A generator expression's or a
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
    in a coroutine (`asyncio.create_task(asyncio.sleep(1))` waits elsewhere). The walk leaves a
    waiting block by its exception edge too, since an exception may skip the wait.
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
  - `NaturalLoop.len_caps` (`LenCap`) records `while len(x) < N` caps up to 4096
    (`LEN_CAP_MAX`); `LoopCarriedInfo.updates` lists the real updates of a loop-carried value.
  - `IrFunction.starred` marks starred display elements, `BasicBlock.term_span` the source of a
    block's terminator (blocker lines point at it), and `ModuleIr.constants`
    (`ModuleConstants`) the module's constant integers, displays, strings, and tuples of
    strings. A string constant, or a tuple of them, is one entry: its text (`strs` /
    `str_tuples`, one `Arc<str>` per string the module writes, which aliases and items share),
    whose length gives its display.
  - The Soundness lane reads these facts within one module only (`FnIndex`). The Advisory rules
    read other modules' small displays through the declarations (`ModuleDecls.small_globals`,
    `ClassDecl.attr_displays`), so the language server re-checks their users when they change.
    A module display another module may grow or rebinds (`cfg.QUEUE.extend(more)`, `from cfg
    import QUEUE` and `QUEUE.append(x)`, `cfg.QUEUE = []`; `ModuleDecls.grown_globals`, by the
    shared `FnIndex::grown_vars`) is not fixed for the Advisory rules, in its own module or
    any other (`ProjectDecls::grown_elsewhere`, a declaration key the language server tracks).

---

## 2. Advisory / Effect lane (`heki_effect`): families 1 and 2

### 2.1 Light receiver type resolution (for the call graph only)
No type errors are reported; resolution only finds call targets, the classes of attributes, and
the targets of write and read boundaries.

| Priority | Source | Confidence | Example |
|---|---|---|---|
| 1 | Local constructors and direct imports (relative imports, `src/` layout, re-exports) | Exact | `requests.get(u)`, `client = httpx.Client(); client.get(u)` |
| 2 | PEP 484 annotations (parameters, locals, return types, attributes), container item types, and return annotations along the MRO; `isinstance` narrowing and unions of up to three classes | Typed | `def f(repo: UserRepo, db: Session): db.execute(...)`, `for m in models: m.save()` with `models: list[Model]` |
| 3 | `self.attr = Cls(...)` in `__init__` and similar | Typed | `self.http = httpx.Client()`, then `self.http.get(...)` in another method |
| 4 | Same-name methods in the class hierarchy (CHA / RTA); a project `Protocol` dispatches to its structural implementations | Heuristic | Concrete subclasses of an abstract base (more than 8 candidates: unresolved) |

A method called on an instance is the definition the class's MRO (Python's C3 linearization of
its project bases) finds first, plus the overrides of its project subclasses (Priority 4). A
named class is exactly that class: constructing it runs the `__init__` its MRO finds, and
`Base.method(obj)` the `method` it finds; `cls` in a class method may be any subclass, so
`cls()` and `cls.method()` dispatch like a call on an instance. A method's calls on its own
`self` run the overrides of every subclass, so a mixin's summary holds the effects of every
class sharing it; an execution effect reached only through the override of a subclass records
that class (`EffectSummary::receiver_effects`), and a call on a receiver of known class runs it
only when that class and the receiver's derive one from the other (an unknown receiver runs them
all). An async client sharing a command mixin with a sync one runs the async implementation.
Calling an `async def` that no decorator replaces gives a coroutine (`Ty::Coroutine`): its return
annotation is what awaiting it gives (`await`, and the target of `async with`), and awaiting a
future or a task gives an untracked result. The catalog types an external awaitable by what
awaiting it gives.

Module names come from project-relative paths (`pkg/mod.py` → `pkg.mod`, `pkg/__init__.py` →
`pkg`). A `src/` directory that is not itself a package (no `src/__init__.py`) is a source root:
`src/acme/io.py` is `acme.io`, for regular and namespace packages and for modules directly under
`src/`. A module name that several files share is defined by one of them, in full and
incremental analysis alike: an implementation before its `.pyi` stub, a package
(`pkg/__init__.py`) before a module file of the same name (`pkg.py`, which Python does not import
next to it), then the greater path. A relative import resolves inside the packages it climbs; one
that climbs past the top-level package binds nothing (`ImportError`), except that a project root
holding `__init__.py` is itself the package of the top-level modules.

Layer 2 `return_type` entries type the results of factories (`sqlite3.connect()` →
`sqlite3.Connection`, `asyncio.get_running_loop()` → `asyncio.AbstractEventLoop`). A call that
cannot be resolved is not followed (`unknown_calls`); an external function that Layer 2 does not
list has no effect.

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
    mutated_param_paths:      Map<ParamIdx, Set<AttrPath>>,// attributes / containers changed through parameter i
    changed_param_attrs:      Map<ParamIdx, Set<AttrName>>,// attributes whose objects change (in place or deeper),
                                                           // not just rebound: what a shallow copy shares (MUT202)
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
    returns_self_attrs:       Set<AttrName>,               // @cached_property: self.<attr> objects the result
                                                           // holds by reference, read for nothing else (MUT101)
    lazily_read_self_attrs:   Set<AttrName>,               // self.<attr> read only where it or a self call
                                                           // initializes it lazily (MUT101)
    mutates_self_attrs:       Set<AttrKeyPath>,            // reassignment or in-place change of self.<attr> (indirect too)
    invalidates_cached:       Set<AttrName>,               // del self.<prop>, self.__dict__.pop("<prop>", ...), clear
    typed_field_write_sites:  Set<TypedFieldWriteSite>,    // own and IndirectHelper writes to (class, attr) with
                                                           // kind, chain, and the parameters the written object
                                                           // may be, plus the sites private callees hand over (a
                                                           // public callee reports its own) (MUT201, EFF201)
    typed_field_writes:       Set<(Class, Attr)>,          // every typed write reached through any callee
    typed_params:             Bitset,                      // parameters the function types itself (its sites
                                                           // cover their changes)
    typed_field_read_sites:   Set<TypedFieldReadSite>,     // own reads of (class, attr): attribute, getattr,
                                                           // __dict__; never propagated (MUT203)
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
                                                           // computed names on guarded classes (MUT205)
    unresolved_access_sites:  Set<UnresolvedAccess>,       // reads and method calls named like a strict
                                                           // contract's target on objects of unknown class;
                                                           // never propagated (MUT206, MUT207)

    // Guards: when an effect runs
    exec_guards:              Map<ExecEffect, EffectGuards>, // input tests every occurrence needs; lazy
                                                           // initialization (once per object, from which
                                                           // parameters)

    // Offloading and the sync-driven context
    offloads_blocking_params: Bitset,                      // wrapper running the callable in parameter i on a thread
    sync_driven_only:         bool,                        // internal async def driven only by a sync driver
    unknown_calls:            bool,

    // What iterating the returned object runs (a plain function returning a generator)
    iterated:                 Option<EffectSummary>,       // in the function's own parameters

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
kept until the indexes change; a recording that reads one again adds what computing it read. Contract-only facts
(container entries and `WriteKind::ItemMut`, construction through `ArgOrigin::new_object` and
`TypedFieldWriteSite::construction`, `unresolved_write_sites`) are computed only when
`Contracts::ownership()` is true, that is when write-owners, read-owners, effect-boundaries, or
invalidation contracts are declared. `TypedFieldWriteSite::key` names the entry a write goes to.
`unresolved_access_sites` hold only the names a `read-owners` or `call-owners` contract with
`unresolved = "report"` ends a target with (`Contracts::unresolved_read`,
`Contracts::unresolved_call`), so a summary keeps no read or call no rule asks for.

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
index alone, for a method of any indexed file.

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
of `MUT108` / `MUT110` reports are no summary facts: the rule walks the own summaries and project
calls of the functions below the call (`FileSummary::local_functions` and `call_sites`) down to
the first function whose body makes the change. The language server compares these facts of the
edited file's functions that change a parameter in place (`heki_rules::chain_facts`) and
re-checks the files with such reports above a function whose facts changed. Likewise, while
`MUT111` is selected, it keeps the stale writes `MUT101` finds in each file's methods, with the
classes whose instances run each writer (`heki_rules::stale_writes_by_file`), as of the file's
latest analysis; when they change, the files reading those cached properties are re-checked. A
`MUT113` report shows where other methods change a container (`self_container_change_sites`):
the language server re-checks the files showing a change site that moved, as for a call site. A
place it records keeps what is there (a call's callees, a change's attribute), so that two calls
or changes of one length swapping on a line, which keep every span, count as moved too.

The incremental index also records what summarizing each file read of the project declarations
(`FileSummary::decl_reads`: the modules looked up, the classes whose subclasses were listed, the
property getter names tried), and the language server records what each check read. An update of a
module's declarations reports the keys it changed (the module, and the hierarchy indexes that
differ); every file that read one of them is re-analyzed. This covers names resolved through
re-exports, class hierarchy analysis across modules, and property getters, without depending on
which module imports which.

### 2.3 Family 1 (`MUT101` / `MUT103` / `MUT106` / `MUT107` / `MUT109` / `MUT111` / `MUT112` / `MUT113` / `MUT201` / `MUT202` / `MUT203` / `MUT204`)

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

#### `MUT111`: readers of a stale `@cached_property` (opt-in)
1. Reads: the `TypedFieldReadSite`s of each checked function's own body (`self.p`, `obj.p`,
   `getattr(obj, "p")`, `obj.__dict__["p"]` / `.get("p")`), whose class `K` heki knows.
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
       `obj.__dict__["a"] = ...`;
     - **container mutation through an alias**: `xs = obj.a; xs.append(...)` / `xs[i] = ...` /
       `xs.clear()` / `xs.sort()` / `xs |= ...` (through alias tracking and
       `mutated_param_paths`, **handing `obj.a` to `helper(obj.a)` that calls `.append()` is
       caught across calls**).
  2. `typed_field_write_sites` propagate bottom-up with their call chains, from private callees
     only: a public callee reports its own writes. A write is allowed
     when a function on its chain matches `allowed-writers` or `exempt-methods`, when an
     initializer of the target's class family writes its own `self`, or when the reporting
     function only runs inside allowed contexts (every path of project callers starts in one); a
     private helper with project callers hands its report to them. Otherwise the outermost
     function is reported, with `message` as the report's help.
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
     (`readonly::subscript_step`, shared with the return flow). A copy method of a project class
     (`Image.copy`) is a call of that method, not a copy by its name (`readonly::call_derivation`).
     A copy keeps its original's class for receiver typing. A phi joins the tags of all its
     incoming values, to a fixpoint.
  3. A direct attribute write or container mutation of a tagged value, or passing it in a
     parameter position the callee changes (`mutated_param_paths` minus `memo_param_paths`; for a
     shallow copy, below the attributes `changed_param_attrs` names), is reported as **`MUT202`**
     with the call hop. Filling a memo of the shared object or initializing one of its attributes
     lazily (`SelfAttrWrite::fills_memo`, `lazy_init`, on any object a parameter or a tagged value
     holds; through a call, a change of the object a memo-named attribute holds) is no change:
     what the object computes stays the same. A change below a shallow copy
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

### 2.4 Family 2 (`EFF101`–`EFF103`, `EFF105`–`EFF107`, `EFF201`, `DATA101`): precision, sanitizers, and aggregation
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
     sliced or computed from the previous array or frame makes it work entry by entry.
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
8. **`EFF201`** (contract): a breadth-first search over the IR-free call graph from each scope
   function finds the shortest chain to each forbidden effect, without entering
   `allowed-callees`, other scope functions, or bodies that do not run at the call (`async def`
   bodies not awaited, generators not iterated). Its nodes are `(function, Body | Result)`: a
   function returning a generator is entered as what iterating its result runs. The search
   reads no guards: an argument switching off a callee's branch does not stop it.

### 2.5 Layer 3: on-demand analysis of unregistered dependencies (planned)
- **Boundary (strict)**: **never enter the standard library or packages registered in Layer 2**
  (A.7: avoids the `asyncio.StreamWriter.write` → `socket.send` misfire).
- **Limits**: at most 3 hops of exploration, 10,000 lines per module, 200 analyzed modules in
  total.
- **Disk cache key**: `(heki_version, effective_kb_digest, env_packages_digest, package_name,
  package_version, rel_path, file_hash)`.

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
| **OPT120** / **OPT121** | `size-dependent` | `Unique(q)` (no escape, not returned), `ExactType(q, list)`, only operations `deque` supports equally (`DequeEquivalentOps`: no `list(*a)` / `list(**kw)`, no iterator live at a change or consumed by it, iterators used only for their elements, `q = q[1:]` exactly) | `NotConstSmall`, **no index `q[i]` other than the ends (`0`, `-1`)** | S12, `sort()`, $O(n)$ middle indexes, `q.extend(reversed(q))`, `type(iter(q))` |
| **OPT130** | `size-dependent` | Every argument `ValueLoopInvariant`, `SideEffectFree` (no method arguments), `ZeroTripExceptionSafe` (no `*` arguments), the built container used by the membership test only (`ValueLoopInvariant(call)`) | `NotConstSmall` (100 times slower when the loop runs zero times: A.8), not a `ConstSmall` / retry loop, not `tuple(t)` of a tuple | S22, S28, A.8 |

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
`copy.deepcopy`; `"shallow"`: `copy.copy`, `BaseModel.model_copy`, whose `deep=True` copies
deeply unless `update=` hands it values by reference: `MUT202`), `return_type` (factories such as
`sqlite3.connect`), `sync_lock_or_tx` (`threading.Lock`, `Session.begin`: `EFF106`), and
`opaque_to_layer3` (reserved for Layer 3), and `cases`: what the call does instead when one
argument (by position, the receiver not counted, or by keyword) is a given literal, a string
literal with a given prefix or not empty, an instance of a given class, or an integer that sets
a named flag, alone or as an operand of `|` (`pd.read_csv` of a `https://` URL is
`blocking_net`, of an `io.StringIO` nothing; `socket.getaddrinfo(..., flags=AI_NUMERICHOST)`
resolves nothing); the first case that holds sets
the call's `effects` and `return_type`, and an argument that is not passed, or may be passed
through `*` / `**` unpacking, selects none. A boolean flag case (`equals = false`) whose argument
is a parameter of the calling function (`shutdown(wait=wait)`) becomes a guard: the effects it
switches off need the parameter to be true, which the callers decide (`close(wait=False)`). A file lists its `opaque_packages`. Projects add or
override entries with `[[tool.heki.ecosystem]]` (`symbol`, `effects`, `async-safe`,
`offloads-blocking-arg`, `excluded`, `copy-sanitizer`, `return-type`, `sync-lock-or-tx`): an
override sets the keys it lists and keeps the built-in entry's others. A `[[tool.heki.ecosystem]]`
entry naming a project function applies to its summary (the built-in catalog never does: a
package's own source is followed through its bodies): its `effects` are direct effects (chain: the function itself),
`offloads_blocking_arg` a parameter it offloads, `excluded` / `async_safe` hide every / the
blocking effects of the final summary from its callers (`EffectSummary::declared_hidden`), and
its `return_type` types the call over the annotation. An `async_safe` entry
keeps only its loop-sensitive effects when the catalog is loaded (built-in entries carry no
blocking effect at all).

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
