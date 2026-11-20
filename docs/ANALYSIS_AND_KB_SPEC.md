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
(against the `MUT101` false positives found in Stage 0-A).

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
    exc_target: Option<BlockId>,         // where an exception raised in this block goes
}
```

### 1.3 Instruction set
Compound expressions are flattened into **basic instructions in explicit left-to-right order**.

| Category | Instructions | Notes |
|---|---|---|
| Constants and allocation | `Const`, `AllocBuiltin(kind, elems)`, `DictLiteral(entries)` | `list` / `dict` / `set` / `tuple` / `str` / `bytes` / number literals, comprehensions, unshadowed builtin constructors; `DictLiteral` keeps the order of `**` unpacking (`{**d, ...}` vs `{**other, **d}`) |
| Names | `LoadGlobal`, `LoadName`, `LoadDeref`, `StoreGlobal`, `StoreDeref`, `Copy`, `Delete`, `Import` | `Copy` adds to the direct alias set, `Delete` removes by strong update; `Import` / `LoadGlobal` carry the PEP 810 `is_lazy` flag |
| Operators | `BinOp(op, lhs, rhs, in_place)`, `UnaryOp`, `Compare(op, lhs, rhs)` | Augmented assignment is `BinOp` with `in_place`; `in` / `not in` are `Compare`; until types are exact, any special method may run |
| Attributes and items | `AttrLoad`, `AttrStore`, `AttrDelete`, `SubscriptLoad`, `SubscriptStore`, `SubscriptDelete` | Stores make the stored value `Shared`; `AttrDelete` recognizes `@cached_property` invalidation (`del self.prop`) and `MUT201` writes; subscripts keep constant keys and slice bounds |
| Calls | `Call(callee, args, kwargs, is_awaited)`, `MethodCall(recv, method, args, kwargs, is_awaited)` | `is_awaited` separates awaited calls from synchronous ones; `star_from` is the index of the first `*` argument, so arguments bind to parameters exactly up to it |
| Iteration and generators | `MakeIter(iterable)`, `IterNext(iter)`, `MakeGenExpr(outer_iter, captured)`, `MakeClosure(captured)` | `MakeIter` and `MakeGenExpr` bind a **derived reference** of `iterable` / `outer_iter` to an explicit SSA variable |
| Suspension | `Await`, `Yield`, `YieldFrom` | `Await` lets other tasks run (`Unique` stays unique); `Yield` makes the yielded value `Shared` |
| Context managers | `EnterWith(mgr, is_async)`, `ExitWith(mgr, is_async)` | Mark the region of a `with` / `async with` (the lock and transaction regions of `EFF106`) |
| Exceptions | `CatchException(types)` | Starts an exception handler |

### 1.4 Lowering, fallbacks, and `LoopBound`
- **`for` / `async for` headers**: for `for x in expr:`, **`expr` (`for.iter`) and `it =
  MakeIter(expr)` are placed in the block before the loop (loop depth `d`)**; the loop body
  (depth `d + 1`) starts with `IterNext(it)` (this removed, by construction, the 7 Stage 0-A
  false positives of `EFF102` / `EFF103` that took the one-time `for.iter` call for a call in the
  loop).
- **Comprehensions and generator expressions**: when
  `RuntimeProfile.pep709_inlined_comprehensions` is true (3.12+), comprehensions are inlined into
  the enclosing frame; generator expressions are separate functions created by `MakeGenExpr`.
- **A1 fallback**: a function with `a1_hazards` is excluded **from `heki_soundness` only**;
  **`heki_effect` (family 1 `MUT`, family 2 `EFF` / `DATA`) continues**.
- **`LoopBound`** (never a Soundness premise; used for profitability and Advisory noise only):
  - `ConstSmall(k)`: a `for` over a small display (below) or `range(k)` / `range(a, b)`
    spanning at most 8, also through `enumerate`, `zip`, `reversed`, `sorted`, `map`,
    `filter`, and `.items()` / `.keys()` / `.values()`, and with range bounds that are constant
    integer expressions (`+`, `-`, `*`, module constants included).
  - `RetryPattern`: a retry body (a `try` that leaves the loop on success and sleeps in a
    handler) over a small constant range or a target named like `retry` / `attempt`, or a
    `while` loop with a retry body that sleeps.
  - `DaemonPolling`: `while self._running:` / `while not self._stopped:`-style flag loops, and
    `while True:` with a retry body that sleeps (resident workers and reconnect loops).
  - `Unbounded`: everything else.
- **Size facts** (`heki_ir::facts`, the one home of size and trip-count facts; profitability and
  Advisory noise only, never Soundness premises):
  - A **small display** (`SmallDisplay`) is a list, tuple, set, or dict display of at most 8
    items (`SMALL_MAX`) without `*` / `**` unpacking that nothing grows or lets escape.
    Shrinking it and passing it to read-only builtins are fine; a store at an unknown index
    (`xs[k] = v`, possibly a slice) counts as growth.
  - `NaturalLoop.len_caps` (`LenCap`) records `while len(x) < N` caps up to 4096
    (`LEN_CAP_MAX`); `LoopCarriedInfo.updates` lists the real updates of a loop-carried value.
  - `IrFunction.starred` marks starred display elements, `BasicBlock.term_span` the source of a
    block's terminator (blocker lines point at it), and `ModuleIr.constants`
    (`ModuleConstants`) the module's constant integers and displays.
  - The Soundness lane reads these facts within one module only (`FnIndex`). The Advisory rules
    read other modules' small displays through the declarations (`ModuleDecls.small_globals`,
    `ClassDecl.attr_displays`), so the language server re-checks their users when they change.

---

## 2. Advisory / Effect lane (`heki_effect`): families 1 and 2

### 2.1 Light receiver type resolution (for the call graph only)
No type errors are reported; resolution only finds call targets, the classes of attributes, and
the targets of write and read boundaries.

| Priority | Source | Confidence | Example |
|---|---|---|---|
| 1 | Local constructors and direct imports (relative imports, `src/` layout, re-exports) | Exact | `requests.get(u)`, `client = httpx.Client(); client.get(u)` |
| 2 | PEP 484 annotations (parameters, locals, return types, attributes) | Typed | `def f(repo: UserRepo, db: Session): db.execute(...)` |
| 3 | `self.attr = Cls(...)` in `__init__` and similar | Typed | `self.http = httpx.Client()`, then `self.http.get(...)` in another method |
| 4 | Same-name methods in the class hierarchy (CHA / RTA) | Heuristic | Concrete subclasses of an abstract base (more than 8 candidates: unresolved) |

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
    escapes_params:           Bitset,                      // stored in an attribute / container / global, or returned
    consumes_params:          Bitset,                      // iterated to exhaustion on every normal path (MUT106)
    unconditional_forwards:   Bitset,                      // passed on at calls that run on every normal path
    iterates_params:          Bitset,                      // stepped by an iteration on some path: a generator
                                                           // passed there runs there
    reads_self_attrs:         Set<AttrKeyPath>,            // self.<attr> and self.<dict>[const_key] reads
    returns_self_attrs:       Set<AttrName>,               // @cached_property: self.<attr> objects the result
                                                           // holds by reference, read for nothing else (MUT101)
    mutates_self_attrs:       Set<AttrKeyPath>,            // reassignment or in-place change of self.<attr> (indirect too)
    invalidates_cached:       Set<AttrName>,               // del self.<prop>, self.__dict__.pop("<prop>", ...), clear
    typed_field_write_sites:  Set<TypedFieldWriteSite>,    // writes to (class, attr) with kind, chain, and the
                                                           // parameters the written object may be (MUT201, EFF201)
    typed_field_read_sites:   Set<TypedFieldReadSite>,     // own reads of (class, attr): attribute, getattr,
                                                           // __dict__; never propagated (MUT203)
    returns_protected:        Option<ProtectedReturn>,     // may return a protected value: producer, depth,
                                                           // call chain (MUT202)
    must_invalidate:          Bitset,                      // invalidation contracts announced on every
                                                           // normal path (MUT109)
    unmet_invalidation:       Set<UnmetWrite>,             // target writes, own or passed up by private
                                                           // helpers, some normal path leaves unannounced

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

`CallSite.returned` (whether the call's result may be returned, and how) and `CallSite.flow` (the
invalidation contracts announced after the call on every normal path, and the next call; set on
`Iterate` / `Through` sites too), with `FileSummary.flows`, are computed at extraction from the
CFG and its post-dominator tree, only when a contract is configured; `ProjectIndex` keeps no IR.

**Guards.** An effect inside a branch of a test of the function's own inputs (`if cache:`,
`if x is None:`, `isinstance(x, T)`, also through `and` / `or` / `not`, early returns, and
conditional expressions; on a parameter, or an attribute of one the function never assigns)
records that test. An effect inside a lazy initialization (`MUT107`'s branch, when every way out
of it sets the attribute) runs once per object. At each call the callee's guards are mapped to
the caller (`through_call`, wherever a callee's facts reach a caller): a constant argument, the
literal default of an omitted parameter, or a test the caller has made on the same value that
decides a guard against the effect prunes it there; an undecided test of a caller input becomes
the caller's guard. A generator's guards are decided at the call that creates it. `EFF102` /
`EFF103` do not count a lazily initialized effect per iteration unless a data-sized loop around
the call constructs the object. Anything not recognized keeps the effect.

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
  at the call, or whose result is iterated there: its `Result` runs inside that call.

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
decorated so), and its body never runs at the call.

Early Cutoff compares summaries without their source positions, so an edit that only moves code
changes no caller's facts. The positions callers copy (call chains, write sites) are still
re-propagated into their summaries, so that the incremental index always equals a full build, and
the language server re-checks the files whose diagnostics show a moved call site. The call chains
of `MUT108` / `MUT110` reports are no summary facts: the rule walks the own summaries and project
calls of the functions below the call (`FileSummary::local_functions` and `call_sites`) down to
the first function whose body makes the change. The language server compares these facts of the
edited file's functions that change a parameter in place (`heki_rules::chain_facts`) and
re-checks the files with such reports above a function whose facts changed.

The incremental index also records what summarizing each file read of the project declarations
(`FileSummary::decl_reads`: the modules looked up, the classes whose subclasses were listed, the
property getter names tried), and the language server records what each check read. An update of a
module's declarations reports the keys it changed (the module, and the hierarchy indexes that
differ); every file that read one of them is re-analyzed. This covers names resolved through
re-exports, class hierarchy analysis across modules, and property getters, without depending on
which module imports which.

### 2.3 Family 1 (`MUT101` / `MUT103` / `MUT106` / `MUT107` / `MUT109` / `MUT201` / `MUT202` / `MUT203` / `MUT204`)

#### `MUT101`: mutation of attributes a `@cached_property` depends on, without invalidation (strict Stage 0-A version)
1. For every `@cached_property` method `p` of a class `C` (with its subclasses; a nested class is
   a class of its own), take `reads_self_attrs(p)`: the `self.<attr>` and constant-key
   `self.<dict>["k"]` reads of `p`, directly and through `self.helper()` calls, without `p`
   itself.
2. For every method `m` of `C` and its subclasses, take the writes in `m`'s own body, **except**:
   - **initialization and two-phase initialization hooks**: `__init__`, `__post_init__`,
     `model_post_init`, `__setstate__`, `__enter__`, `__new__`, `setup`, `setUp`, `postinit`,
     `_open`, `_setup`, `_init_*`, `_configure_*`, and private helpers that an initializer calls;
   - **memo and cache dictionaries**: attributes named like `_cache` / `_memo`, `setdefault`, and
     lazy filling under `if k not in self._xxx:`;
   - **disjoint constant dict keys**: when the keys `p` reads (`self.params["outtmpl"]`) and the
     keys `m` writes (`self.params["proxy"]`) are all constant and disjoint.
3. Report **`MUT101`** when an attribute in `reads_self_attrs(p) ∩ writes(m)` remains and **`p ∉
   invalidates_cached(m)`** (and `m` has no full invalidation such as `self.__dict__.clear()`; a
   private helper all of whose callers invalidate is covered).
4. For an attribute in **`returns_self_attrs(p)`**, only rebinding writes count (`self.a = ...`,
   `del self.a`): the cached value holds the attribute's object itself and sees in-place changes.
   The fact is local to the getter (every read of `self.a` is returned as itself or put, as
   itself, into a display or a container that is returned; `self` reaches no other code), minus
   the attributes the `self` methods and properties it calls read (final summary). A starred
   display element (`(*self.a,)`) is a copy: the IR lowers it to a copy named `.unpacked`.

#### `MUT103`: cross-function effect snapshots (`heki snapshot summary`)
- The summaries of public functions (`mutates_params`, `escapes_params`, `mutates_self_attrs`,
  and `direct_exec_effects ∪ transitive_exec_effects`) are saved in a normalized form
  (`.heki-summary-snapshot.json`); a pull request whose functions gained effects or mutability is
  reported.

#### `MUT106`: single-pass iterators consumed twice (opt-in)
- Sources: generator expressions; `iter` / `zip` / `map` / `filter` / `enumerate` / `reversed`
  and the single-pass `itertools` functions (never `tee`, `repeat`, `cycle`, `count`); project
  generator functions; and `Iterator` / `Generator` annotations (not `Iterable` / `Sequence`).
- Consumers: `for` loops and comprehensions, unpacking, `yield from`, and the calls that
  materialize an iterable (`list` / `tuple` / `set` / `frozenset` / `dict` / `sum` / `any` / `all`
  / `min` / `max` / `sorted` / `str.join`, `list.extend`, `set.update`, `collections.deque`,
  `heapq.nlargest`, `functools.reduce`, ...: one table, `consumption::FULL_CONSUMERS`, which also
  decides where a generator's body runs), also through wrappers over one iterable; `next()` is
  stepping, not consumption. A project function consumes its argument when `consumes_params` says
  so, and its body runs at the call.
- The first pass must be full (a loop that can `break`, `return`, or `raise` is partial); the
  second pass must be reachable from it on normal CFG paths; an iterator created outside a loop
  and consumed inside it is reported unless the loop runs at most once.

#### `MUT107`: attributes `__hash__` depends on
- `reads_self_attrs` of `__hash__` (with its `self` helpers) against the own writes of every other
  method of the class and its subclasses, with `MUT101`'s initialization, memo, and constant-key
  filters, writes directly under a `None` / falsy guard of the same attribute (lazy
  initialization), the attributes `__hash__` caches itself (`self._hash`), and stored-hash names.

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
  2. `typed_field_write_sites` propagate bottom-up with their call chains. A write is allowed
     when a function on its chain matches `allowed-writers` or `exempt-methods`, when an
     initializer of the target's class family writes its own `self`, or when the reporting
     function only runs inside allowed contexts (every path of project callers starts in one); a
     private helper with project callers hands its report to them. Otherwise the outermost
     function is reported, with `message` appended.
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
     `model_copy(deep=True)`, and the Layer 2 `copy_sanitizer` entries).
  3. A direct attribute write or container mutation of a tagged value, or passing it in a
     parameter position the callee mutates (`mutates_params`), is reported as **`MUT202`** with
     the call hop.
  4. **Return flow**: a backward walk from the returns marks each call whose result may be
     returned (as itself, as an element, or as an item of target unpacking; a copy ends it). A
     callee that is a producer, an `@lru_cache` function, or `returns_protected` makes the caller
     `returns_protected`, so a wrapper returning a producer's result is a producer itself, across
     modules. The join is "protected if any path returns it", and the shortest chain is kept.
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
     dict named `requests` is not mistaken for the package (type and name resolution).
   - **`EFF103`**: `for.iter` headers excluded; SQL literals that are DDL (`CREATE` / `ALTER` /
     `DROP` / `PRAGMA`) and queries in migration functions excluded; the internals of ORM
     libraries excluded.
   - **`DATA101`**: only `df = pd.concat([df, ...])` / `arr = np.vstack([arr, ...])` whose SSA
     `PhiNode` accumulates across the back edge (`LoopCarried`); one-off concatenation and padding
     in a loop are excluded.
4. **Duplicate aggregation (boundary-only reporting and root-cause grouping)**:
   - In `async A → await async B → sync C → blocking D`, only the first crossing into the
     synchronous world (`async B`) is reported; when more than a threshold (10) of callers reach
     the same root cause, they are grouped into one representative with the count.
5. **Context relaxation**: `click` / `typer` commands (`@command`), ASGI lifespan hooks,
   `LoopBound::ConstSmall` / `RetryPattern` loops, and test code (`lint.relax-async-in-tests`,
   default true) are relaxed by default.
6. **`EFF106`** (opt-in): a forward must-hold dataflow over the CFG for `with` regions of
   synchronous locks (Layer 2 `sync_lock_or_tx` classes: `threading.Lock` / `RLock` / `Semaphore`
   / `BoundedSemaphore` / `Condition`) and transaction starters (`Session.begin()`,
   `Connection.begin()`, `Engine.begin()`, `transaction.atomic()`, `psycopg.Connection.transaction()`,
   ...); an `await` or a call site reaching `BlockingNet` inside the region is reported.
7. **`EFF107`** (opt-in): a `create_task` / `ensure_future` result bound to a named local is
   reported when no alias of it is awaited, returned, yielded, stored, put in a container, passed
   to a call, or captured; being only the receiver of `add_done_callback` / `set_name` does not
   keep it, `cancel()` / `result()` / `exception()` do.
8. **`EFF201`** (contract): a breadth-first search over the IR-free call graph from each scope
   function finds the shortest chain to each forbidden effect, without entering
   `allowed-callees`, other scope functions, or bodies that do not run at the call (`async def`
   bodies not awaited, generators not iterated). Its nodes are `(function, Body | Result)`: a
   function returning a generator is entered as what iterating its result runs.

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
| **OPT102** | `unconditional` | `Unique(target)`, `ExclusiveAccessAtWrite(target)`, `ExactType(target / rhs, set \| dict)`, **for dict unpacking the accumulated `d` comes first (`{**d, ...}`): `DictUnpackLeading`**, `ExceptionStateSafe`, `EvalOrderPreserved` | `LoopCarried(target)` | S26, S26b, **datamodel-code-generator `_target_binding.py:465` (`{**other, **found}` accumulates on the right and reverses the override order)** |
| **OPT103** | `size-dependent` | `Unique(acc)`, `ExclusiveAccessAtWrite(acc)`, `ExactType(acc / rhs)`, `NoIntermediateReads`, `ExceptionStateSafe` | `LoopCarried(acc)`, `NotConstSmall` (local `str +=` excluded) | A.8 (slower for small loops), `self.s +=`, `__radd__` on the right |
| **OPT110** | `size-dependent` | `ValueLoopInvariant(lst, loop)`, `ExactType(lst, list \| tuple)`, hash-consistent builtin elements (`HashConsistentProbe`), `ZeroTripExceptionSafe` | `NotConstSmall(lst)` | S9a, S9b, S22, S27 |
| **OPT111** | `size-dependent` | `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, list)`, hash-consistent builtin elements, only the appends the set mirrors (`OnlyPairedAppends`) | `NotConstSmall` | `seen.remove`, unhashable elements |
| **OPT120** / **OPT121** | `size-dependent` | `Unique(q)` (no escape, not returned), `ExactType(q, list)`, only operations `deque` supports equally (`DequeEquivalentOps`) | `NotConstSmall`, **no index `q[i]` other than the ends (`0`, `-1`)** | S12, `sort()`, $O(n)$ middle indexes |
| **OPT130** | `size-dependent` | Every argument `ValueLoopInvariant`, `SideEffectFree`, `ZeroTripExceptionSafe`, the built container never changed | `NotConstSmall` (100 times slower when the loop runs zero times: A.8) | S22, S28, A.8 |

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
`asgiref.sync.sync_to_async`), `excluded` (`logging.Logger.*`), `copy_sanitizer`
(`copy.deepcopy`, `BaseModel.model_copy`: `MUT202`), `return_type` (factories such as
`sqlite3.connect`), `sync_lock_or_tx` (`threading.Lock`, `Session.begin`: `EFF106`), and
`opaque_to_layer3` (reserved for Layer 3). A file lists its `opaque_packages`. Projects add or
override entries with `[[tool.heki.ecosystem]]` (`symbol`, `effects`, `async-safe`,
`offloads-blocking-arg`, `excluded`, `copy-sanitizer`).

The entries follow the public API of each package, with three conventions:
- An async API is `async_safe` and carries no blocking effect: it is often called without a
  direct `await` (`async with`, `asyncio.gather`, `asyncio.wait_for`), where the `is_awaited`
  filter does not apply. Its loop-sensitive effects (`db_query`, `unpooled_client_alloc`) stay.
- A constructor or factory that does no I/O until first use (`redis.Redis`, `pymongo.MongoClient`,
  `sqlalchemy.create_engine`) carries only `unpooled_client_alloc`; a call that only builds an
  object (`Redis.pipeline`, a lazy `QuerySet.filter`, `Session.query` and the `Query` builder
  methods) carries only a `return_type`. Iterating an object runs its class's `__iter__` entry
  (`sqlalchemy.orm.Query.__iter__`): a `for` loop, a comprehension, or a generator expression over
  a value of that class is a call of it at the iterable (`async for` is not).
- `db_query` is one statement sent to the database and waited for: reading the rows of an
  executed statement (`sqlite3.Cursor.fetchone`) is not one, nor is a driver's `commit()`; an ORM
  `Session.commit()` / `flush()` sends the pending writes and is.

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
  count`) and Tachyon `--gecko` JSON are read into one table of hot functions; `heki check
  --profile` uses it to apply proven size-dependent rewrites in hot functions.
- **Constraints checked on real tools**: Tachyon on CPython 3.15 cannot combine `--opcodes` with
  `--jsonl` (`--gecko` / `--binary` are needed), and macOS needs `sudo` to read another process's
  memory. Profile integration is therefore never required, only an optional add-on for tuning.

---

## 5. Stage 0-A measurement results (completed 2026-11-19)

339 repositories (118 mature open-source repositories with 12.42 million lines and 221 new GitHub
repositories of 2026 with 5.68 million lines; 56,629 files, 18,108,143 lines in total) were
scanned with 8 workers (62.8 s), and all 171 samples of the 8 rules were reviewed by hand:

| Rule | Hit rate by domain | Indirect share | Conclusion of the 171-sample review | Decision in Baseline v4.0 |
|---|---|---|---|---|
| **EFF101** | Web/async **37.6%** (35/93) / DB 29.1% | **82.3%** (436/530) | **68% real harm** (including the real `time.sleep` in redis-py `cluster.py:1613`, reached in 4–5 hops) | **Core** (awaited calls and `Popen` excluded) |
| **EFF102** | Web/async **44.1%** (41/93) / DB **48.5%** (50/103) | **90–93%** | **About 75–80% real harm** after the `for.iter` fix (NeMo, OmniAgent, luigi, ...) | **Core** (`for.iter` and `DaemonPolling` handled) |
| **EFF103** | DB **47.6%** (49/103) / web 31.2% | **74.2%** (630/849) | Many N+1 queries; DDL, migrations, and ORM internals cause noise | **Core** (`for.iter`, DDL, ORM internals excluded) |
| **DATA101** | Data/ML **19.5%** (24/123) | — | All 60% false positives of a naive AST check were non-`LoopCarried` padding; with `LoopCarried` **about 90% precise** | **Core** (SSA `LoopCarried` required) |
| **MUT101** | Mature **16.1%** (19/118) / new 0.9% | — | Hit a real bug in datamodel-code-generator 0.86.0, and in mkdocs, wagtail, textual, ...; the about 64% noise of the naive version came from 3 causes | **Adopted in its strict form** (two-phase initialization, memo dicts, disjoint dict keys excluded) |
| **MUT102a/b** | Mature 4.2% / new 0.5–1.4% | — | Sparse and mostly deliberate idioms (zero-copy `Sequence`, `frozen` accumulating lists) | **Rejected by measurement** (datamodel-code-generator's **`MUT201` / `MUT202`** adopted instead) |
| **DATA102** | Data/ML 12.2% / mature 2.5% | — | A single-expression syntax check that duplicates Ruff / pandas-vet | **Rejected** |

*(The Wave 2 candidates were measured the same way: `MUT107`, `OPT112`, `OPT113`, `EFF106`,
`MUT106`, and `EFF107` are implemented; `MUT104` for `ALL_CAPS` constants, `EFF108`, and `MUT105`
were rejected; `OPT131` still waits for its measurement.)*
