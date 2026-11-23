# heki Architecture (Baseline v4.1)

| Item | Content |
|---|---|
| Status | **Experimental** (pre-release; rules, configuration, and output can still change). Architecture Baseline v4.1, updated on 2026-11-20 for the implemented v1.x rules (46 rules). Based on the Stage 0-A Wave 1/2 measurements (about 19 million lines), a study of datamodel-code-generator's 20,000 lines of in-house checkers, one CLI and configuration with Ruff / `t-linter`, the incremental LSP design, and PEP 810 support verified on `ruff_python_parser` 0.17.5. |
| Date | 2026-11-20 |
| Target language | Python (verified on CPython 3.11–3.15, including PEP 709, PEP 649 `__annotate__`, and PEP 810 `lazy import`) |
| Implementation language | Rust: a single binary with a CLI and an incremental LSP server, distributed with `maturin`. Only the frontend uses `ruff_python_parser` / `ruff_python_ast` (`tag = "0.17.5"`), isolated in `heki_ast`. |
| Related documents | [Scope and limitations](docs/scope.md) / [Design history and ADRs](docs/DESIGN_HISTORY_AND_ADR.md) / [Analysis engine, IR, and knowledge base](docs/ANALYSIS_AND_KB_SPEC.md) / [Agent rules](AGENTS.md) |

Sections marked **(planned)** describe designed parts that are not implemented yet; everything
else describes the code as it is.

---

## 1. Product definition and positioning

**heki** (壁, Japanese for "wall") is an experimental static checker for Python that tracks side
effects and mutations across function calls: a wall where dynamic typing leaves the code unchecked. It checks **what code does, not
what types it has**: which objects it changes in place, which references it shares, and which
blocking, network, or database work it reaches. What it adds lasts as the type checkers grow:
aliases and in-place changes followed through any helper or subclass, contracts on who may
write, read, or call what, and rewrites that come with a proof. It is the **third layer of
Python static analysis: an Effect (execution and state change) and Alias (uniqueness and
ownership boundaries) semantic checker**.

| Layer | Typical tools | Underlying model | Coverage and limits |
|---|---|---|---|
| **1: syntax and local linting** | Ruff, Flake8 | AST and one file | Style and syntax bugs. No calls across functions, aliases, side effects, state changes, or control flow. |
| **2: type checking** | ty, mypy, Pyright, Pyrefly | Type lattice | Type consistency. No aliasing, escapes, write ownership, or I/O effects. |
| **3: Effect & Alias semantic checking (this project)** | **heki** | **Effects and aliases over an SSA control-flow graph and the call graph** | **State corruption across function and class boundaries, write-ownership violations, stale caches, blocking calls, and $O(n^2)$ complexity regressions** |

*(Import dependencies between layers of modules and import cycles belong to tach /
import-linter and to Python 3.15+ PEP 810 `lazy import`; heki stays with effects and aliases.)*

### 1.1 What the product is: semantic checks first, speedups as a by-product

- **Main value (from mature production code to web and AI code)**: catch in CI what Ruff (syntax)
  and ty (types) cannot see in principle: state corruption across function and class
  boundaries, write-ownership violations, mutation of shared objects, stale caches, blocking
  I/O, and clients re-created in loops. No custom decorators: standard `typing`, zero-config
  inference, boundaries declared in `pyproject.toml`, and a Ratchet baseline.
- **Speedups as a by-product (strong where the pattern exists)**: the same analysis yields (1)
  production speedups by removing event-loop stalls, clients and database queries in loops,
  and quadratic DataFrame concatenation, and (2) safe automatic fixes and proven suggestions for
  $O(n^2)$ patterns once uniqueness, derived references, and exact types are proven.
- **One theory, no ad-hoc rules**: no framework-specific heuristics (`__getattr__` proxies,
  Pydantic model validation, `-X importtime` tuning, and the like). Every rule belongs to one of
  **three rule families** derived from the single Effect & Alias theory. The gate for new rules
  is in [docs/scope.md](docs/scope.md).

### 1.2 Two lanes on one IR, three rule families

On a shared frontend and SSA-CFG IR sit **two analysis lanes of different nature**, separated
by crate boundaries, carrying three rule families.

1. **Advisory / Effect lane (`heki_effect`)**: call-graph analysis that uses standard `typing`,
   `self` attributes, and a light class hierarchy.
   - **Family 1: Mutation & State Effect (`MUT`)**:
     - Inferred (`MUT1xx`): mutation of attributes a `@cached_property` depends on without
       invalidation, effect regressions against a saved snapshot, attributes `__hash__` reads
       changing after initialization (`MUT107`), and single-pass iterators consumed twice
       (`MUT106`, opt-in).
     - Declared boundaries (`MUT2xx`): **write ownership**, **read ownership**, and **read-only
       returns** declared in `pyproject.toml`, checked across functions and through aliases.
   - **Family 2: Execution Effect (`EFF` / `DATA`)**: event-loop stalls in `async` code,
     clients and connection pools re-created in loops, explicit database queries in loops,
     loop-carried quadratic `pd.concat` / `np.vstack`, `await` or network I/O while a
     synchronous lock or a database transaction is held (`EFF106`), tasks kept only by a weak
     reference (`EFF107`), a non-reentrant lock taken again while it is held (`EFF109`), an
     object allowing one operation at a time used by tasks started together (`EFF110`), the
     iteration order of a set of strings reaching an output across a call (`EFF112`), a
     `RecursionError` a broad `except` turns into a value (`EFF114`), an `async def` handed to a
     library call that never awaits what it returns (`EFF115`) (all opt-in), and **effect
     boundaries** declared in `pyproject.toml` (`EFF201`).
2. **Soundness / Alias lane (`heki_soundness`)**: conservative abstract interpretation that never
   trusts type annotations (zero unsafe rewrites).
   - **Family 3: Complexity Rewrite (`OPT`)**: behavior-preserving $O(n^2) \to O(n)$ rewrites
     based on uniqueness (frame locality), derived references, exact types, and the invariance
     of object contents. `OPT112` and `OPT113` belong to the family but are Advisory-lane
     reports without a rewrite.

> **Why the two lanes are separate crates**
> Automatic rewriting (Soundness) must never trust type annotations (a subclass is a
> counterexample) and must fall back to `Unknown` whenever anything is unclear. Cross-function
> effect and mutation detection (Advisory) finds about 60–70% of its candidates from name
> resolution alone, but the remaining 30–40% (synchronous database sessions reached through
> method calls, write boundaries, state changes inside classes) need a best-effort resolution
> that uses PEP 484 annotations, `self` attributes, and the class hierarchy. The crate boundary
> makes it a compile-time error for `heki_soundness` to depend on `heki_effect`.

---

## 2. Goals and non-goals

### Goals
- **G1. Zero configuration, no code changes**: no custom imports, decorators, or type
  annotations. Standard `typing` and inference only, plus optional boundaries declared in
  `pyproject.toml`; no run-time overhead at all.
- **G2. One theory, no ad-hoc rules**: every diagnostic derives from the Effect & Alias theory on
  the SSA-CFG and the call graph; no framework-specific guesses.
- **G3. Honest reasons, separate safety and profitability**: behavior preservation (safety) and
  the certainty of a speedup (profitability) are managed as orthogonal axes; nothing without a
  reason, and nothing that can be slower for small inputs, is applied by default.
- **G4. Gradual adoption and machine-readable output**: a line-number-independent, shrink-only
  baseline (`heki check --baseline`) and JSON output carrying the evidence (`evidence`), the
  obstacle to a fix (`blocker`), and the call path (`related`; SARIF carries the call path as
  `relatedLocations`), for large existing code bases
  and for the fix loops of humans and agents in CI.
- **G5. Coexistence with existing tools**: no overlap with Ruff, ty, mypy, Pyright, or tach;
  `# noqa` compatible.

### Non-goals
- Custom contract APIs in user code (`Owned`, `Borrowed`, `move()`, `@pure`, ...).
- Theorem provers such as Lean 4.
- Constant-factor micro-optimizations (hoisting `re.compile` included).
- **General import-layer boundaries and import cycles** (left to tach / import-linter and Python
  3.15+ PEP 810 `lazy import`; heki stays with effects and aliases).
- **Single-expression syntax lints, dead code, and duplicated code** (`df.iterrows()`, unused
  variables, duplicated functions: Ruff / vulture / CodeQL).
- **Ahead-of-time native extensions (mypyc / C / Rust bridges) and build-time rewriting** (loading
  `.so` files at CLI start-up and compatibility overhead make them counterproductive; staying
  pure Python is a premise).
- **Framework-specific heuristics** (`__getattr__` proxies, Pydantic model construction,
  `-X importtime` tuning, and other application-architecture topics).
- **Forcing profiler runs or instrumenting processes** (reading an existing profile dump is an
  optional add-on).
- Changing CPython, its bytecode, or its VM; run-time hooks; being a judge for Ruff's unsafe
  fixes; a general type checker.

---

## 3. Semantics, safety, and profitability

### 3.1 Observational equivalence
A Soundness-lane rewrite preserves meaning when these four agree before and after it:

1. **The function's result**: the return value (equal, with corresponding object structure), or
   the **type** of the raised exception.
2. **The trace of external calls**: the order of calls into code outside the analysis or
   unknown code, and the state of the objects reachable from their arguments and from globals
   at each call.
3. **Changes to shared state**: the order and content of destructive changes to `Shared`
   objects.
4. **Termination**: normal exit, exception, or an infinite loop.

**Not observed** (implementation-defined even by the language): run time, memory use, the number
of allocations, reference counts, the values of `id()`, traceback line numbers, and the message
strings of built-in exceptions (their types are preserved).

**Observed, although the language leaves it open**: the iteration order of a `set`. CPython fixes
it for given elements and history (an `int` hashes to itself in every run), and whatever
iterates, converts, formats, or returns the set shows it. Copying a set into a new table
(`s | t`) and growing its table in place (`s |= t`) can order the same elements differently, so a
rewrite from one to the other proves that nothing observes the order (`SetOrderUnobserved`). A
dict iterates in insertion order, which both keep.

### 3.2 Assumptions (A1–A5)

| ID | Name | Content | How heki treats it |
|---|---|---|---|
| **A1** | Basic assumption | (a) No frame introspection reads or changes locals from outside (`sys._getframe`, `f_locals`, trace / monitoring hooks, `gc.get_objects`, outside manipulation of a generator's `gi_frame`). (b) A builtin name refers to the real builtin unless it is shadowed within the analyzed code. (c) Nothing depends on unobserved behavior. (d) `MemoryError`, the recursion limit, and the position of signal interrupts are not preserved. | **Always assumed in the Soundness lane.** A function containing `locals()`, `eval()`, `exec()`, `sys._getframe`, and the like is excluded **from the Soundness lane only** (the Advisory lane continues). `heki assumptions` lists the violating places of a project. |
| **A2** | Static module bindings | A function or class defined once at module level is not replaced at run time | Needed by a conservative `SoundnessSummary` for interprocedural rewrites **(planned)**; not assumed by the default fixes. Symbols that tests replace with mocks (`mock.patch`, `monkeypatch`, ...) are never covered by A2. Today the Soundness lane is intraprocedural. |
| **A3** | Correct native knowledge base | The Layer 1 descriptions of C-level builtins and stdlib primitives match their real behavior | Verified by sentinel tests. Entries whose `verified_max_python` is older than the target version become `Unknown` in the Soundness lane. |
| **A4** | Exact declared annotations | `x: list[int]` is an exact builtin type, not a subclass | Never assumed by default (unrelated to the Advisory lane's type resolution and `MUT` checks). |
| **A5** | No dependence on finalization timing | Nothing depends on when `__del__` or weak-reference callbacks run | Required only by rules that change an object's lifetime. |

### 3.3 Orthogonal safety and profitability

Automatic rewrites (family 3, `OPT`) are judged on two orthogonal axes: **behavior preservation
(safety obligations)** and **the certainty of a speedup (profitability: `LoopCarried`,
`NotConstSmall`, size dependence)**.

- **Safety tiers**: `safe` (every safety obligation is `Established`, assuming A1 only) /
  `conditional` (needs A2–A5) / `blocked` (something is unproven; a `blocker` is reported) /
  `advisory` (families 1 and 2: semantic warnings without a rewrite) / `disproved` (a
  counterexample exists; the diagnostic is suppressed).
- **Profitability tiers**:
  - `unconditional`: always faster, from an input of size $n = 1$ and from zero iterations
    (**only `OPT101` and `OPT102`**).
  - `size-dependent`: for small inputs or loops that run zero times, building or converting the
    new structure costs more (**`OPT103`, `OPT110`, `OPT111`, `OPT112`, `OPT113`, `OPT120`,
    `OPT121`, `OPT130`**).
  - `semantic-advisory`: semantic diagnostics about state corruption, write boundaries, stale
    caches, blocking, clients in loops, and the like (families 1 and 2).

#### Automatic fixes (`heki check --fix`) and realistic expectations
- The default `heki check --fix` applies **only `safety == safe` and `profitability ==
  unconditional` (`OPT101`, `OPT102`)**, and only for the rules that run: while `OPT101` and
  `OPT102` are heuristic, a run without configuration applies none (select them, or set
  `lint.heuristic = true`).
- A `safe` but `size-dependent` fix is offered by default as a **suggestion proven to preserve
  behavior**, and applied only with `--include-size-dependent`, or in functions an external
  profile dump (`--profile`) shows hot.
- Measured: the default `heki check --fix` applies very rarely (about 20 provable cases in 13.2
  million lines of mature code, 1 in 6.5 million lines of new code, 0 in datamodel-code-
  generator's 196,000 lines). heki's everyday CI value is in **family 1 (mutation, state, and
  ownership), family 2 (execution effects), and the proven suggestions of family 3**.


### 3.4 Exact and heuristic reports (assumptions R1–R7)

A report's claim is what its message states about the code, in the terms its rule page defines
under "What a report states" ([ADR-018](docs/DESIGN_HISTORY_AND_ADR.md#adr-018)). The claim is a
conjunction of **premises**: a call resolves to the function shown; a function reaches the next
call on every path on which it returns; a statement runs on every iteration of a loop that
completes; a loop steps through a named input (a parameter, an attribute of one, the rows of a
query); a library call has its effect under its condition; a store is a plain store; two names
hold the same object. Whether the code does harm, and whether a rewrite of it is valid, are not
premises.

- **Exact**: a report is built only from established premises. A premise that is open, a budget
  that runs out, or a fact heki lacks stops the report. A wrong exact report is a defect of one
  premise's fact, fixed in that fact with a counterexample, never by a list of exclusions in the
  rule.
- **Heuristic**: some premise can be open without stopping the report: a branch heki cannot
  decide counts as taken, a loop not shown to run a call once counts as running it on every
  iteration, a size heki cannot bound counts as data-sized (not fixed at 16 or fewer). The rule
  page names the premises a heuristic report may not have established.
- **Codes and tiers**: a claim is one code. An effect rule whose claim needs a premise heki
  cannot always establish gets a heuristic sibling code with the weaker claim ("can reach"
  beside "reaches on every path"). An `OPT` rule keeps one code and a certainty per report: a
  report whose rewrite is proven safe is the rule's exact tier, the others are heuristic. A
  report is exact only when its rule is (`Certainty::at_most`).
- **Size is never claimed by an exact report.** A bound of 16 or fewer that the code states
  only removes reports. A larger fixed bound, or a size heki cannot bound, is no evidence: an
  exact loop claim names the input the loop steps through instead.
- **Messages**: an exact report's message states its claim and the witnesses of its premises
  (the calls of the chain, the input the loop steps through), nothing else: no cost estimate
  such as "(N+1)", and no advice, which goes into the help.
- **Default**: a rule runs by default when it is exact and its share of real harm is worth the
  line it prints; an exact rule with little harm is opt-in.

An exact claim holds in every run consistent with these assumptions, as A1–A5 are those of a
rewrite:

| ID | Assumption | How exact rules treat it |
|---|---|---|
| **R1** | Static bindings: the functions and classes on a call chain are not replaced at run time (no monkeypatching) | Where the project rebinds a name heki can see (a definition chosen by a test, a module-level `setattr`, a `mock.patch` target outside tests, a module `__getattr__`), the call does not resolve for an exact premise |
| **R2** | A value annotated `C` is an instance of `C` or of a project subclass | Assumed by the reporting rules only; the Soundness lane and `--fix` never assume it (ADR-004, A4). A must-fact through a method call holds only when every candidate implementation has it: a project subclass that overrides the method (of a project class or of a library class, such as `post` on a subclass of `httpx.Client`) leaves the premise open. A type fact keeps its provenance (constructed here, or an annotation under R2), so that a report resting on R2 can be dropped where annotations are not trusted |
| **R3** | The ecosystem catalog (Layer 2) describes the library: an entry's effect, and the condition under which it has it | An effect with a condition (`queue.Queue.put` waits only on a bounded queue) is established only when its condition is |
| **R4** | Imports resolve as configured (`src`, `known-first-party`, the layout) | An import that does not resolve gives no fact |
| **R5** | The checked files are the project: code outside them reaches a module only through its public names | A fact about the callers of a private function counts only the checked files. A receiver typed by a `Protocol`, or by a union with a `Protocol` member, stays unresolved: its structural implementers are never a complete set |
| **R6** | A library call's argument is a waiting value unless heki establishes otherwise, and so are the arguments that opened the connection it runs on | A catalog entry that waits lists the argument values for which it returns at once (`time.sleep(0)`, a numeric host for `getaddrinfo`); a call whose argument is established as one of them is silent, and an argument heki cannot evaluate counts as waiting, a SQLAlchemy engine's or another DB-API connection's URL included. `sqlite3.connect` is the one exception, as its in-memory form is common: its connection waits only when heki establishes that the database argument names a file (`names_file`) |
| **R7** | No asynchronous or resource-exhaustion exception (`MemoryError`, `KeyboardInterrupt`, a signal) is raised: without this, nothing would run "whenever" anything else does, as such an exception can arise at any instruction (A1(d) for rewrites) | Under R1, R3, and R4, a load of a defined local, of a module name bound once, of a builtin the module does not rebind, or of a member of an imported module, and a `list` or `tuple` display, do not raise; other attribute loads, subscripts, `set` and `dict` displays, calls, awaits, and operators may |

"A branch heki cannot decide can go both ways" is not an assumption: it is what makes a report
heuristic. One more case is a choice of silence, not an assumption: a context manager heki does
not resolve may suppress exceptions, so an exact fact does not pass through its body after an
instruction that may raise.

---

## 4. Analysis theory and engine design

### 4.1 Shared foundation: the SSA-CFG IR (`heki_ir`) and semantic analysis (`heki_semantic`)
The parser (`ruff_python_parser`) feeds heki's own scope, binding, and class-hierarchy
resolution (`heki_semantic`); then an SSA-CFG IR is built for every function.

- **Current syntax and scope rules**:
  - Python 3.12+ PEP 709 (inlined comprehensions), 3.14+ PEP 649 (`__annotate__` scopes), and
    **3.15+ PEP 810 (`lazy import` / `lazy from ... import ...`, `is_lazy=1`: the binding is
    materialized on first use, not when the module loads)** are modeled exactly, driven by
    `RuntimeProfile`.
- **Explicit exception edges and evaluation order**: implicit control edges from instructions
  that can raise, and left-to-right evaluation.
- **Lowering**: `for` (the anonymous iterator becomes an explicit SSA variable through `MakeIter`
  and `IterNext`; **the `for.iter` header expression stays outside the loop body**), `while`,
  `with` / `async with`, `try` / `except` / `else` / `finally`, comprehensions, generator
  expressions (`MakeGenExpr`), augmented assignments, unpacking, `match`, and f-strings (each
  replacement field converted and formatted where it stands, `Convert` and `Format`, then
  joined, `BuildString`) become basic instructions.
- **First-class `await` / `yield`**: suspension points are explicit, and awaited calls are
  distinguished from synchronous calls.
- **Classes, decorators, and loops**: natural loops with a `LoopBound` (`ConstSmall(k)` /
  `RetryPattern` / `DaemonPolling` / `Unbounded`), nested classes kept apart from their outer
  class, and decorator metadata (`@cached_property`, `@lru_cache`, CLI and web decorators).
- **Size facts in one home** (`heki_ir::facts`): small containers (displays, constant
  strings, builtin constructors, comprehensions, constant slices, lists filled a fixed number
  of times), constant trip counts (also of `while` loops over a counter the code bounds),
  length tests on every path to a loop, loops paced by a sleep or a timed wait on every
  iteration (polls and retries), and `while len(x) < N` caps, for profitability and Advisory
  noise only. The Soundness lane reads
  them within one module (`FnIndex`); the Advisory rules read other modules' small displays
  through the declarations (`ModuleDecls.small_globals`, `ClassDecl.attr_displays`).
- **Original byte spans**: fixes are produced as minimal `TextEdit`s, never by regenerating code
  from the IR, so formatting and comments survive.
- **Bounded walks in one home** (`heki_ir::walk`): the engines every analysis walk goes through
  (`Memo`, `Closure`, `reach`, `fixpoint`, and `SharedMemo` over the declarations), a step
  `Budget` per owner, the tables of one function (`FnShape`), and `Answer` (`Known | Open`),
  heki's one type for an answer that is established or unknown. Their memos live in their owner
  for one analysis, never in `ProjectIndex`, which stays `Sync` (ADR-017).

### 4.2 Advisory / Effect lane (`heki_effect`): families 1 and 2

#### Light type resolution for the call graph
No type errors are reported; types are resolved only to find call targets, the classes of read
and written attributes, and the targets of write boundaries. Priority: (1) local constructors and
direct imports, (2) PEP 484 annotations, (3) `self.attr` assignments in `__init__` and similar,
(4) the class hierarchy (CHA, at most 8 candidates). A call that cannot be resolved is not
followed (the function is marked `unknown_calls`), and an external function that the Layer 2
catalog does not list contributes no effect.

Receiver types are inferred by a fixpoint over the SSA-CFG: annotations, including container
item types (`list[Model]`, `dict[str, Model]`, `tuple[...]`, `Iterator[...]`); the items taken
out by loops, subscripts, unpacking, dict views, and the builtins that wrap iterables; property
getters' and methods' return annotations along the MRO, as a type checker reads them; `Self`;
copies; `functools.partial`; `typing.cast`; unions of up to three classes; `isinstance`
narrowing; the closure cells of enclosing functions, which hold every value the function binds
to them unless a nested scope rebinds them; `type(obj)` and a `type[C]` / `typing.Type[C]`
annotation, a class object that may be a subclass as `cls` is; and the structural
implementations of project `Protocol`s (up to eight).

Reading a property is a call: `obj.attr` on an instance of a project class whose `attr` is a
getter-only `@property` / `cached_property` (found along the MRO and in overriding subclasses, at
most 8) is a call site of the getter that binds the receiver, at the read. A property with a
setter or deleter is not modeled, because the setter shares the getter's qualname and summary.
`FunctionDecl.is_property` is a declaration fact, so `ProjectIndex` stays IR-free. Calling a
property's value, `obj.attr(...)`, is that read and a call of what the getter returns, typed by
its return annotation (`FunctionDecl.property_type`, also for a property with a setter).

#### One `EffectSummary`, propagated bottom-up over Tarjan SCCs
For the project's functions, a worklist fixpoint in reverse topological order of the call
graph's Tarjan SCCs propagates **execution effects, mutation and state effects, and write
ownership** in one summary. Members of a component are visited in Tarjan's pop order (callees
first), so simple cycles converge in two rounds; a component that exceeds 16 recomputations per
member is stopped and its members are marked as not fully known.

```text
EffectSummary(f) = {
    // Family 2: execution effects
    direct_exec_effects:      Set<ExecEffect>,      // BlockingNet, BlockingDb, BlockingSubprocess,
                                                    // BlockingSync, Sleep, BlockingFs,
                                                    // UnpooledClientAlloc, DbQuery, QuadraticRealloc
    transitive_exec_effects:  Set<ExecEffect>,
    exec_chains:              Map<ExecEffect, ShortestCallChain>,

    // Family 1: mutation and state effects (shared by MUT1xx / MUT2xx)
    mutates_params:           Bitset,               // parameter i (or its attributes / elements) is changed
    escapes_params:           Bitset,               // parameter i is stored in an attribute / container or returned
    consumes_params:          Bitset,               // parameter i is iterated to exhaustion (MUT106)
    unconditional_forwards:   Bitset,               // parameters passed on at calls that always run
    mutated_param_paths:      Map<ParamIdx, Set<AttrPath>>, // attributes / elements changed through parameter i
                                                    // ("[]": below its items, also through a shallow copy)
    reads_self_attrs:         Set<AttrKeyPath>,     // reads of self.<attr> (constant dict keys included)
    mutates_self_attrs:       Set<AttrKeyPath>,     // reassignment or in-place change of self.<attr>
    invalidates_cached:       Set<AttrName>,        // del self.<prop>, self.__dict__.pop("<prop>", ...)
    typed_field_write_sites:  Set<(Class, Attr, WriteKind, Chain)>, // own and indirect writes, and those handed
                                                    // over by private callees (MUT201, EFF201)
    typed_field_writes:       Set<(Class, Attr)>,   // every typed write reached through any callee
    typed_params:             Bitset,               // parameters the function types itself
    typed_field_read_sites:   Set<(Class, Attr, ReadKind)>,         // own reads only, never propagated (MUT203)

    exec_guards:              Map<ExecEffect, EffectGuards>, // input tests every occurrence needs; lazy init
                                                    // (once per object, from which parameters)
    capacity_waits:           Vec<CapacityWait>,    // own queue waits whose capacity the callers decide

    // Offloading and the async context
    offloads_blocking_params: Bitset,               // wrapper that runs the callable in parameter i on a thread
    iterates_params:          Bitset,               // stepped by an iteration: a generator passed there runs there
    iterated:                 Option<EffectSummary>, // what iterating the returned object runs

    // Locks (EFF109; only with the rule selected)
    locks:                    Option<LockFacts>,    // non-reentrant locks taken on the function's
                                                    // thread or task, in its names (self.<attr>,
                                                    // a global, a closure variable), with the chain,
                                                    // the input guards, and the tests of state on
                                                    // the way; the globals and closure variables
                                                    // it may bind
    exclusive:                Option<Map<ObjRef, ExclusiveUse>>, // objects used one operation at a time
                                                    // on the function's task, unless a lock is
                                                    // held (EFF110; only with the rule selected)
    set_order:                Option<SetOrderFacts>, // where the function takes a set's order, where
                                                    // its sources' orders go (return, outputs,
                                                    // call arguments), and, propagated, what it
                                                    // returns in a set's order and which parameters
                                                    // reach an output (EFF112; only with the rule
                                                    // selected)
    call_cycle:               Option<CallCycle>,    // none, a function it calls is on a call cycle,
                                                    // or itself (EFF114; only with the rule selected)
    sync_driven_only:         bool,                 // driven only through a sync driver such as run_sync(coro)
    unknown_calls:            bool,                 // unresolved own call, CHA past 8, or an A1 hazard
    shared_state:             Option<SharedStateFacts>, // module globals and enclosing functions' closure
                                                    // cells changed, here, in a body run here, or by a
                                                    // callee handed them (heki inspect and the server)

    // Metadata: is_async, is_generator, is_cached_property, is_lru_cache, owner class, decorators
    // kept: Option<Box<KeptFacts>> (MUT101 only): must_read_self_attrs (the self.<attr> read on
    // every path that returns normally), removes_kept, and listed_cache (the decorator is a
    // [[cached_properties]] entry)
}
```

#### Precision and noise control from real code (Stage 0-A: 339 repositories, 18.1 million lines, plus datamodel-code-generator's 196,000 lines)
1. **User-defined offload wrappers**: besides `asyncio.to_thread`, thin project wrappers such as
   `await in_thread(self.open)` propagate `offloads_blocking_params` bottom-up (finding from
   datamodel-code-generator `bodies.py:112`), and functions decorated with an offloading API
   (`@sync_to_async`, `@database_sync_to_async`, `@asyncify`) are calls of that API. Such a
   function, and an offloading API's callable, run on a worker thread: everything they do
   counts at the call except blocking effects.
2. **Sync-over-async relaxation**: an internal `async def` driven only through a synchronous
   driver is relaxed for `EFF101` / `EFF105` (finding from datamodel-code-generator
   `client.py:1399`).
3. **The six false-positive sources found by the Stage 0-A review**:
   - `for.iter` / `async for.iter` headers count as outside the loop (`EFF102` / `EFF103` no
     longer report one-time execution).
   - Awaited calls (such as `await redis.delete(...)`) and non-blocking spawns (`subprocess.Popen`
     itself; `.wait()` / `.communicate()` do block) are excluded from `EFF101`.
   - `EFF103` excludes DDL (`CREATE TABLE` / `CREATE INDEX`) run by a DB-API `execute`, also
     inside helpers and when the statement is built at run time from a DDL start (`f"ALTER
     TABLE {t} ..."`) or taken from a module's schema constants, migration helpers, and the
     internals of ORM implementations.
   - `EFF102` recognizes and relaxes the polling loops of resident daemons (reconnect loops such
     as `while self._running:` or `while not stop.is_set():`) and loops that wait on every
     iteration (polls and retries).
   - `DATA101` requires an SSA **`LoopCarried`** value (accumulation through reassignment) and
     excludes one-off concatenation or padding in a loop, and loops whose trip count the code
     fixes.
   - `MUT101` excludes (1) two-phase initialization hooks (`__init__` / `__post_init__` /
     `model_post_init` / `_init_*` / `_configure_*` / `setup_*` / `_open`, ...), (2) memo
     dictionaries (`_cache[k] = v`), (3) disjoint constant dict keys (`params["proxy"]` vs
     `params["outtmpl"]`), (4) the separate `__init__` of a nested class, (5) lazy
     initializations that the cached property runs itself, and (6) classes with a metaclass
     other than `type` / `abc.ABCMeta`, which may turn attribute writes into descriptor calls.
4. **Staged `EFF101` and duplicate aggregation**: the default covers `BlockingNet` / `BlockingDb`
   / `BlockingSubprocess` / `BlockingSync` / `Sleep`; `BlockingFs` is the opt-in `EFF105`;
   `logging` / `print` are excluded. An `async` chain is reported only at its boundary, and
   output folds more than 10 boundary reports sharing a root cause into one, after inline
   suppressions and the Ratchet baseline. Click / Typer commands, ASGI lifespan hooks,
   `ConstSmall` / `RetryPattern` loops, and test code are relaxed by default.

### 4.3 Soundness / Alias lane (`heki_soundness`): family 3

Datalog was not adopted ([ADR-006](docs/DESIGN_HISTORY_AND_ADR.md#adr-006)): the lane runs an
**RPO worklist abstract interpretation over a provenance-carrying lattice** on the SSA-CFG and
evaluates **declarative proof obligations (three-valued: `Established` / `Blocked` /
`Disproved`)**.

- **Uniqueness, derived references (`derived_refs`), and frame locality**:
  - The implicit iterator of `for v in x:` (`MakeIter`), generator expressions (`MakeGenExpr`),
    and Layer 1's `retained_in_return` (`iter`, `enumerate`, `zip`, `map`, `filter`, `reversed`,
    `dict.keys/values/items`, `memoryview`, ...) are tracked as derived references of the
    original object. An escaping derived reference transitively demotes the original object to
    `Shared`.
  - At the rewritten statement, every direct alias and every derived reference other than `x`
    must be dead (`ExclusiveAccessAtWrite`; counterexamples S23–S26b).
  - A CFG merge of different `alloc_id`s stays `Unique` with up to 4 allocation sites
    (`MAX_JOIN_ALLOC_IDS`); more makes it `Shared`.
- **Invariance of object contents in a loop (`ValueLoopInvariant`)**: besides the variable not
  being rebound (`BindingLoopInvariant`), the value is an immutable builtin, or a mutable builtin
  that stays `Unique` through the whole loop and is never changed in it (counterexamples
  S27–S28).
- **A conservative `SoundnessSummary` for A2 (planned)**: computed inside `heki_soundness`, with
  no dependency on `heki_effect`, only for statically bound functions (defined once at module
  level, not mocked). Until then, every call into project code counts as unknown.

---

## 5. Ecosystem knowledge and version differences (three layers)

At start-up, the configuration and the target Python version are resolved (`heki_env`: the
`[tool.heki]` table or `heki.toml`, and `requires-python`), and the knowledge bases are
projected once onto flat tables without a version dimension, so the analyses never branch on
versions.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ Layer 1: native Soundness knowledge base (kb/soundness/*.toml)               │
│  builtins, collections, itertools, memoryview, ...                           │
│  - used only by the Soundness lane (never holds advisory effects)            │
│  - exact return types, parameter effects (retained_in_return, ...)           │
│  - every entry checked by sentinel tests; entries beyond                     │
│    verified_max_python become Unknown                                        │
├──────────────────────────────────────────────────────────────────────────────┤
│ Layer 2: ecosystem effect catalog (kb/ecosystem/*.toml)                      │
│  public standard-library APIs (socket, subprocess, time, io, pathlib,        │
│  sqlite3, threading, asyncio [async-safe / offloads], logging [excluded])    │
│  plus major packages (requests, httpx, aiohttp, sqlalchemy, django, psycopg, │
│  redis, boto3, pandas, numpy, ...)                                           │
│  - declarative model of public APIs for the Advisory lane (the origin of     │
│    every effect): effects, async safety, offloaded arguments, copy           │
│    sanitizers, factory return types, synchronous locks and transactions,     │
│    argument cases (a literal argument selects effects or the return type),   │
│    sync facades (`run_sync`), and lazy iterators                             │
│  - user additions and overrides with [[tool.heki.ecosystem]]                 │
├──────────────────────────────────────────────────────────────────────────────┤
│ Layer 3: on-demand analysis of unregistered dependencies (planned)           │
│  - today: every module of the run is analyzed; functions of other code       │
│    that Layer 2 does not list have no known effects                          │
│  - planned: analyze unregistered pure-Python dependencies (in-house shared   │
│    packages, ...) on demand, never the standard library or Layer 2 packages  │
└──────────────────────────────────────────────────────────────────────────────┘
```

- **No dives into the standard library or Layer 2 packages**: `asyncio.StreamWriter.write(data)`
  reaches the non-blocking `socket.send` in two hops (measured in appendix A.7 of the first
  evaluation). Keeping effects out of Layer 1, and never letting the analysis enter the standard
  library or Layer 2 packages, removes this class of false reports on correct async code by
  construction.

---

## 6. Rules (three families from one theory)

The adopted, rejected, and candidate rules are fixed by the Stage 0-A measurements (339
repositories, 18.1 million lines, all 171 samples reviewed by hand), the Wave 2 measurements
(19 million lines), and the study of datamodel-code-generator's 20,000 lines of in-house checkers.
The 46 rules are listed in the [rules reference](docs/rules/index.md).

### Family 1: Mutation & State Effect (`MUT`: hardening mature production code)

| Rule ID | Default | What it finds | Main noise control and configuration |
|---|---|---|---|
| **MUT101** | Opt-in (experimental, exact) | **Mutation of attributes a `@cached_property` depends on, without invalidation**: a write of a `self.<attr>` (a plain rebind or delete, or an in-place change of the builtin list, dict, or set it holds by its type) that a cached property reads on every path on which its getter returns normally (itself, or through `self` calls and property reads whose targets are every implementation), with nothing from the write, its own instruction included, to the method's normal return that may remove or replace the kept value (a store or deletion of the name through any reference not established as an object keeping none, `__dict__` and `__class__` changes, awaits and yields, calls that may run project code, by `[[invokes]]` for library calls, and implicit code: special methods heki does not establish as `object`'s, a builtin type's, or a library's handed no project instance on every class an operand may be); the getter's decorator is one the catalog lists as keeping the value in the instance `__dict__` (`[[cached_properties]]`). Its other reports (a read on some paths, a call that may remove the value, a decorator outside the list, an initialization hook recognized by name, a setup step: a finalizer, a method `__init__` calls, lazy setup through a second cached property) stay out of the exact tier; opt-in because the real-harm share of its exact reports is below 10% | Two-phase initialization hooks (`model_post_init` / `_init_*` / `_configure_*` / `setup_*` / `_open`, ...), memo dicts (`_cache[k] = v`), disjoint constant dict keys, the separate `__init__` of nested classes, lazy initializations the cached property runs itself, and classes with a metaclass other than `type` / `abc.ABCMeta` (it may turn attribute writes into descriptor calls) are excluded (Stage 0-A findings); a class that names nothing under the property's name holds no value of it |
| **MUT103** | With a snapshot (opt-in, heuristic) | **Cross-function effect and mutability regressions in CI (`heki snapshot summary`)**: a pull request adds new parameter mutations, escapes, or execution effects (direct or transitive) to a function fixed in the snapshot | Zero-config snapshot comparison |
| **MUT106** | Opt-in (heuristic) | **A single-pass `Iterator` / `Generator` consumed twice**: an iterator that SSA does not rebind (generator expressions, `iter` / `zip` / `map` / ..., project generator functions, `Iterator` / `Generator` annotations) is consumed a second time and silently yields nothing | Reported only when the second consumption is reachable in the CFG from the first (exclusive branches never are); a partial first pass (`break`, `any` / `all`) and `next()` stepping do not count; an iterator created outside a loop and consumed inside it is found too; consumption by a called function is followed through the summary |
| **MUT107** | Opt-in (heuristic) | **Attributes `__hash__` depends on changing**: a method other than the initialization hooks (subclasses included) rebinds or changes in place, with a plain store, a `self.<attr>` that the `__hash__` of its instances reads, each `self` call in the hash resolved along the writer's MRO (an instance already in a `set` or a `dict` is filed under its old hash) | The `MUT101` filters (two-phase initialization, memo dicts, disjoint constant keys), plus lazy initialization under a `None` guard and the cache attributes of `__hash__` itself (`self._hash`, ...); silent where the write may run code (a property, `__setattr__` / `__delattr__`, a metaclass other than `type`, a base heki names no class for) (Wave 2: 21.2% of mature projects) |
| **MUT108** | Opt-in (heuristic) | **In-place changes to read-only fields**: a field of a `@dataclass(frozen=True)`, `NamedTuple`, or `dataclass_transform(frozen_default=True)` class, a `Final` / `ReadOnly` attribute, or a `ReadOnly` `TypedDict` key changed in place, directly, through an alias, or through a helper (type checkers stop only rebinding) | Rebinding and `del` (type checkers' domain), `setattr` / `object.__setattr__` / `__dict__` bypasses, initializers and helpers that only run inside them, instances the function constructs itself, `field(compare=False)` fields, value-side changes (dcg: 4 reports, all correct and deliberate; Stage 0-A: 99 field reports, deliberate in-place updates of frozen records, hence opt-in) |
| **MUT109** | When configured (contract, heuristic) | **Announced changes**: a write to a `[[tool.heki.invalidation]]` target (directly, through an alias, or through a helper) must reach one of the contract's invalidators on every path to the function's normal return, directly, in each branch, behind a boolean flag set after the write, or through calls; private helpers pass the obligation to their callers, and a function every caller path of which announces the change after the call is not reported | Writes a helper makes through an argument it does not type count at the call; an initializer is exempt only for its own `self`. Exception paths, objects the function constructs, copies, or gets from a call, `exempt-writers`; deferred context managers are not followed (dcg: every current site passes; 1 design-level report, `DataType.replace_reference`) |
| **MUT110** | On (exact, `warning`) | **Read-only parameters changed by a helper**: a `Sequence` / `Mapping` / `ReadOnly[...]` parameter (after `Optional` / `\| None` / `Annotated`) handed, as itself or through a local alias, to a function that changes it in place, directly or through further helpers (type checkers trust the helper's signature, so an unannotated or `Any` helper hides the change), reported at the call with the chain and the line of the change, which changes the caller's object whenever it runs (a `warning`: a bug that is rarely harmful) | Shares `MUT108`'s analysis; changes in the function itself (type checkers), value-side changes, copies, helper parameters annotated mutable or read-only, variadic parameters (Stage 0-A: 5 reports, all genuine; datamodel-code-generator and the heki corpus: none) |
| **MUT111** | Opt-in (experimental, exact) | **Readers of a stale `@cached_property`**: a function whose own body reads a cached property (on `self` or on any object of the class, also through `getattr` and `__dict__`) that a `MUT101` write leaves stale on the instances the object may be, reported once per function and property with the writers as related locations: under a Ratchet baseline, a new reader of a value known to go stale is one new diagnostic; exact when every class the object may be runs a write whose `MUT101` report is exact, its other reports outside the exact tier; opt-in because the real-harm share of its exact reports is below 10% | `MUT101`'s findings, whatever is selected (no second notion of staleness); a writer counts for the instances that run it (the first definition along the MRO, or one reached through `super()` by overrides that do not invalidate); the functions calling a reading getter or helper are not reported (datamodel-code-generator: 11 readers, all of the three stale properties; the corpus: 38 in mkdocs, of two `File` properties its setters leave stale) |
| **MUT112** | Opt-in (experimental, heuristic) | **A container that is a new object on one path and borrowed state on another, changed in place**: a value made by the function (a display, a comprehension, a copy, a constructor, an operator, a project function returning a new object) on one path and a parameter, attribute, global, enclosing variable, an element of one, or a project function returning such state on another (`items = given or []`, `d.get(k, {})`, `getattr(obj, "tags", [])`), changed in place directly, through a helper, or in a closure; also a store of such a value in `self.<attr>` that a method of the class family changes in place | Values new or borrowed on every path, copies before the change, the accumulator idiom (`if out is None: out = []`) unless the value is also stored on `self`, new objects stored in other state before the paths meet, values stored back where they borrow from, memo attributes; a function's return ownership (new, borrowed, or both, through wrappers) is part of its `EffectSummary` (5 of 5 fixed upstream bugs caught at their parents; datamodel-code-generator: 13 reports, 4 plausible; the corpus: 36, none false, hence opt-in) |
| **MUT113** | Opt-in (experimental, heuristic) | **A container changed while a method iterates it**: a `dict`, `set`, or `deque` in `self.<attr>` (`OrderedDict`, `defaultdict`, and `Counter` included) iterated by a `for` loop or `yield from` (its views and lazy wrappers too) and changed in place by code outside the loop: a method of the class family that can run while a generator is suspended inside the iteration, or a function the loop's body calls; the next step raises `RuntimeError` | What runs while a generator is suspended follows where its object goes: a private generator iterated in place runs only its loops' bodies (their calls on its own instance count), one whose object outlives one use of it (stored, or stepped by `next()` or a loop that leaves early and used again) lets any public method run except two-phase initialization, a public generator is the class's interface (only property getters and read dunders count); snapshots (`list(...)`, `.copy()`), rebinding, item stores on a deque, and changes written in the loop body itself (the linters' check) are excluded; the facts are an optional `EffectSummary::iterations` (redis-py #3889 caught at the fix's parent; no report on datamodel-code-generator, the corpus, or eight OSS heads) |
| **MUT114** | On (exact) | **Cached results changed in place**: the list, dict, or set that a `functools.lru_cache` / `functools.cache` function (resolved by import, `maxsize` not `0`, keyed by a parameter besides `self` / `cls`) returned on every live return, changed in place where it is that function's result on every path, with no copy; its other reports (through wrappers, helpers, or objects below the result) are withheld by default | The `MUT202` copies and memo fills; the container kind by return annotation or construction |
| **MUT115** | Opt-in (heuristic) | **A change below a shallow copy**: an object the function reaches below its own shallow copy (`x.copy()`, `dict(x)`, `{**x}`, `[*x]`, a slice, `copy.copy`, `model_copy()`, `replace`) of state that outlives the call (a parameter, an attribute, a global, an enclosing variable), changed in place directly or through a helper; the copied value holds the same object, so it sees the change. The exact claim (computed per report) needs a builtin container copy by annotation, `copy.copy` of a hook-free project class, or `model_copy()` of a pydantic model; plain loads from the copy that nothing gives a value of its own; and an in-place change of a builtin container or a plain attribute store | The `MUT202` copies (`heki_effect::readonly`) and memo fills; a change of the copy itself, a level rebound before the change on every path or named by `update=` / a `replace` keyword, deep copies, `__dict__` stores, and iterations over the copy are no report (datamodel-code-generator #5705) |
| **MUT118** | With `--heuristic` or by name (heuristic) | **An `object()` sentinel lost in a deep copy**: `copy.deepcopy` of an instance of a class whose own code stores a module-level `object()` sentinel in an attribute (a stdlib dataclass field default its generated `__init__` stores, `self.attr = SENTINEL`, or a field default of a base the catalog says keeps hashable defaults) that a function tests against the sentinel with `is` / `is not`: the copy holds a new object there, which the test does not match; reported at the deep copy, the tests as related locations | No report for a sentinel that copies as itself, a class (or base) with `__deepcopy__` / `__reduce__` / `__reduce_ex__` / `__getstate__` / `__setstate__` or a `copyreg` registration, a `memo` argument, or `==` tests |
| **MUT119** | On (exact, per report) | **A change lost on a new object**: an in-place change of a builtin container (a changing method, an item assignment or deletion) applied to an unnamed new object that nothing else holds: built there (`list(xs).sort()`), a catalogued copy (`os.environ.copy().update(...)`), a slice, or what project functions or a property getter return when every return of each is a new container they keep nowhere (`OptInTables::fresh_returns`); a method returning `None` is reported when its result is unused, bound, or stored | No report for a project class's method, an object bound to a name first, a result the code reads, or a function that may return shared state, keeps the new object, returns `None` on a path, or is cached |
| **MUT201** | When configured (contract, heuristic) | **Declared write ownership**: writes to the classes and attributes listed in `[[tool.heki.write-owners]]` (through local aliases, container operations `.append()` / `[k] =`, `__dict__` / `setattr`, and helpers in other functions) are limited to the allowed modules and classes (stores, services) | Declared in `pyproject.toml` only (no decorators, no run-time cost). `exempt-methods` for construction and copy methods; a class family's initializer may initialize its own `self`; a function that only runs inside allowed contexts is not reported; `message` is appended to the report. With the Ratchet baseline an existing code base migrates step by step (generalizes datamodel-code-generator's `check_generation_store_usage.py`: 1,665 lines, 1,323 lines of tests, and 13 run-time witnesses) |
| **MUT202** | When configured (contract, heuristic) | **Declared read-only returns**: the shared objects returned by the functions listed in `[[tool.heki.readonly-returns]]` (and the containers below them) must not be mutated by callers or downstream functions, including the values returned through wrappers: a function returning a producer's result (unless copied) becomes a producer itself, across modules (`returns_protected`, propagated bottom-up); `@lru_cache` / `@cache` results are `MUT114`'s | Checked statically through cross-function `mutates_params` and alias tracking (replaces the run-time fingerprint monitoring of datamodel-code-generator's `tests/checks/schema_graph.py`) |
| **MUT203** | When configured (contract, heuristic) | **Declared read ownership**: reads of the classes and attributes listed in `[[tool.heki.read-owners]]` (local aliases, `getattr`, and `__dict__` included) are limited to the allowed readers (the read side of `MUT201`) | The same patterns, subclass expansion, initializer rule, `exempt-methods`, delegation from private helpers to their callers, allowed-context rule, and `message` as `MUT201`. Read sites are kept as local facts and the rule walks the reverse call graph, so summaries do not grow with propagated reads |
| **MUT204** | When configured (contract, heuristic) | **Declared call ownership**: calls of the functions, methods (overrides included), and constructions of the classes listed in `[[tool.heki.call-owners]]` outside the allowed callers (or inside `forbidden-callers`), through the receiver's class, bound-method aliases, and constant `getattr` calls | Reported only when every candidate is a target; `MUT203`'s delegation, initializer, and allowed-context rules; targets may call each other; call sites are local `FileSummary` facts (`CallSite::constructs`) |
| **MUT205** | When configured (`unresolved = "report"`, heuristic) | **Writes a `write-owners` contract cannot check**: outside the write owners, an attribute named like a target written on an object of unknown class, or a computed attribute name on a guarded class or on an object of unknown class (`setattr(obj, name, v)`, `obj.__dict__[name]`, `__dict__.clear()`), so a clean `MUT201` run says how much it did not check | Objects of known classes (`MUT201`'s domain), the contract's scopes and baselines |
| **MUT206** | When configured (`unresolved = "report"`, heuristic) | **Reads a `read-owners` contract cannot check**: outside the read owners, a read of an attribute named like a target (also through a local alias, a constant `getattr`, or `__dict__`) on an object of unknown class (an unannotated parameter, a loop variable, an untyped call result, `Any` / `object`, a protocol whose implementations heki cannot list), so a clean `MUT203` run says how much it did not check | `MUT203`'s allowed readers, exempt methods, allowed contexts, and `forbidden-readers`; for an untyped parameter the callers decide: not reported when every caller binds an instance of a project class outside the targets' classes. Access sites are local facts (`EffectSummary::unresolved_access_sites`) |
| **MUT207** | When configured (`unresolved = "report"`, heuristic) | **Calls a `call-owners` contract cannot check**: outside the call owners, a call of a method named like a target (its last name; also through a bound-method alias or a constant `getattr`) on a receiver of unknown class, so a clean `MUT204` run says how much it did not check | `MUT204`'s allowed callers, exempt methods, targets calling each other, allowed contexts, and `forbidden-callers`; the same caller walk as `MUT206`; a call of a value of unknown origin (`callback()`) names nothing and is not reported |

*(`MUT102` (`MUT102a` deep mutation / `MUT102b` representation exposure), measured in Stage 0-A,
is **rejected**: the hits were sparse and mostly deliberate idioms;
[ADR-014](docs/DESIGN_HISTORY_AND_ADR.md#adr-014).)*

### Family 2: Execution Effect (`EFF` / `DATA`: performance and failures of production and AI code)

| Rule ID | Default | Stage 0-A density | What it finds and how precision is controlled |
|---|---|---|---|
| **EFF101** | On (exact) | Web/async **37.6%** (82% indirect) | Blocking calls (network, database, subprocess, synchronization, sleep) reachable from `async def`, directly or indirectly. Offload wrappers and offloading decorators (their queries and writes still count), sync-over-async relaxation, awaited calls and `subprocess.Popen` excluded; `queue.Queue.put` only on a queue known to be bounded (its `maxsize`, followed through locals, attributes, globals, and parameters); a generator is reported where it is iterated, not where it is created; boundary-only reporting with root-cause grouping, `logging` / `print` excluded |
| **EFF102** | Off (heuristic) | Web/async **44.1%** / DB **48.5%** (90–93% indirect) | HTTP clients, sessions, or connection pools created in a loop (across functions too). `for.iter` excluded; `ConstSmall` / `RetryPattern` / `DaemonPolling` loops relaxed |
| **EFF103** | On (exact) | DB **47.6%** (74% indirect) | Explicit database queries or external RPC calls in a loop (N+1, across functions too). `for.iter`, DDL, migrations, ORM internals, and bulk APIs excluded. **Implicit ORM lazy loading is out of scope (known limitation)** |
| **EFF105** | Opt-in (heuristic) | — | Local file I/O reachable from `async def` (`BlockingFs`; `--extend-select EFF105`) |
| **EFF106** | Opt-in (heuristic) | 2.1% (fatal when it happens) | `await` or `BlockingNet` (across functions too) while a type-certain synchronous lock (`threading.Lock` / `RLock` / `Semaphore` / `BoundedSemaphore` / `Condition`) or a database transaction (`.begin()` / `.begin_nested()` / `transaction.atomic()` / ...) is held by a `with`. Async locks and `async with` are out of scope; offloading and test relaxation as in `EFF101` |
| **EFF107** | Opt-in (heuristic) | — | `asyncio.create_task` / `loop.create_task` / `asyncio.ensure_future` results bound to a local variable but never awaited, returned, stored, passed on, or captured (lost to the garbage collector). Discarded results are Ruff's `RUF006` and are not reported; `TaskGroup.create_task` is out of scope |
| **EFF109** | Opt-in (experimental, heuristic) | — | A non-reentrant lock (`threading.Lock`, `asyncio.Lock`, `asyncio.Condition`, `multiprocessing.Lock`) taken again while it is held, by a nested `with` / `acquire()` or by a function the holder calls on its own thread or task, across modules; both sides name the lock the same way (`self.<attr>`, a module global, a closure variable), mapped through each call's arguments (`EffectSummary::locks`), while nothing binds the name to another object. Calls that only create a coroutine or a generator, run on a worker thread, or hand a callable on are not followed; acquisitions a constant argument switches off, or behind a test of the lock owner's state that the holder or its callees change, in the acquiring function or on the way to it (re-entrance flags), and non-blocking or timed acquisitions are not reported |
| **EFF110** | Opt-in (experimental, heuristic) | — | An object allowing one operation at a time (Layer 2 `one_at_a_time` methods: `asyncio.StreamReader` reads, asyncpg `Connection` queries, websockets `recv`, aiohttp WebSocket `receive`) used by tasks a function starts together (`asyncio.gather`, `asyncio.wait`, an `asyncio.TaskGroup`, an anyio task group): two tasks, or one started per item, reach a use of the same object through their callees' `EffectSummary::exclusive_uses`, mapped through each call's arguments to the fan-out function's parameter, global, closure variable, or local. SQLAlchemy's `AsyncSession` only where the fan-out can be its first operation since it was made or committed | Different objects (`stdout` and `stderr`), uses while a lock is held (`CallSite::under_lock`), the item of the loop or comprehension starting each task, a task started again only once it is done |
| **EFF112** | Opt-in (experimental, heuristic) | — | The iteration order of a set of `str` / `bytes` values (by construction, set operators, module constants, or annotation) reaching an output across a call: a value built in the set's order (iterated, copied, formatted, a pick) returned up to the caller that writes it, or handed down to a writer (`EffectSummary::set_order`: each function's order flows to its returns, outputs, and call arguments, resolved against its callees' facts). Outputs are the catalog's `writes_in_order` calls with their sorting switch established off (`json.dumps`, `yaml.safe_dump(sort_keys=False)`, `pprint.pp`, `Path.write_text`, `print`), stream writes, and a raised builtin exception's message | A flow inside one function (a linter's), sets of other values, `sorted()` on the way, order-free uses, a set bounded to one element on every path, one element at a time written to its own file or raised, a mapping read back by key, a key stored again after a read, a set filled in order, `logging` and `warnings`, a sorting switch heki cannot evaluate |
| **EFF114** | Opt-in (experimental, heuristic) | — | An `except` clause taking `RecursionError` (`RecursionError`, `RuntimeError`, `Exception`, `BaseException`, bare, or a tuple with one, as the builtins the module does not rebind), first of its `try` to take it, around a `try` body whose call runs a function on a call cycle (`EffectSummary::call_cycle`: the components of the propagation over calls whose bodies run on the caller's stack, joined over callees), and no path through the clause raises or exits: the function goes on with a value that depends on the stack depth. Not reported: A clause after one that re-raises it or whose classes are unknown, a clause raising on a path, a CLI boundary (the error reported to `stderr` or through `logging` with the exception, an integer constant on every `return`), cycles reached only through a worker thread, a coroutine only created, or a generator only made, `except*`, a `finally` body on the way |
| **EFF115** | Opt-in (experimental, heuristic) | — | An `async def` (directly, as a bound method, or through `functools.partial`) handed to a library call that calls it as a plain function and never awaits what it returns (the catalog's `calls_unawaited` and every `offloads_blocking_arg` API): a thread's, timer's, or process's target, an executor's `submit` / `map`, `loop.run_in_executor`, `asyncio.to_thread`, `loop.call_soon` / `call_soon_threadsafe` / `call_later` / `call_at`, `atexit.register`, `signal.signal`, an exit stack's `callback`, `weakref.finalize`, and the predicates of `filter`, `itertools.filterfalse`, `takewhile`, and `dropwhile`, which test the coroutine for truth (`FileSummary::unawaited_handoffs`, the callees' declarations read at check time). Worded as possible when a decorator replaces the function or another implementation of the method is a plain function | `map`, `starmap`, `reduce`, and sort keys (the coroutines are handed on, or the type checkers report them), a project function or a project subclass of `threading.Thread` taking the callable, async generators, lambdas, classes |
| **EFF201** | When configured (contract, heuristic) | — | **Declared effect boundaries**: functions matching the `scopes` of `[[tool.heki.effect-boundaries]]` must not reach a forbidden effect (`blocking-*`, `sleep`, `blocking`, `mutates-params`, `writes-targets`, `reads-targets`, `calls` of `call-targets`, `reads-environment` of the catalog's `environment` entries and, opt-in, import probes), directly or transitively; `allow-writes` with `write-universe` reports every reached write in the universe that is not allowed (one implementation with `writes-targets`; candidates, not a gate: dcg's pass declarations show over-approximation). Paths through `allowed-callees` and the inside of other scope functions are not followed (boundary-only reporting); the shortest call chain is attached |
| **DATA101** | On (exact) | Data/ML **19.5%** (Stage 0-A audit, 2026-11: about 90% of reports right about the shape with `LoopCarried`) | Loop-carried quadratic `pandas.concat` / `numpy.vstack` / `numpy.append` accumulation. **An SSA `LoopCarried` value is required**: one-off concatenation, padding, and `ConstSmall` loops are excluded |

*(`DATA102` (`df.iterrows()`), measured in Stage 0-A, is **rejected**: it duplicates
single-expression lints.)*

### Family 3: Complexity Rewrite (`OPT`: $O(n^2)$ fixes as a by-product of alias analysis)

| Rule ID | Default | Pattern → rewrite | Profitability | `heki check --fix` | Main safety obligations / profitability conditions |
|---|---|---|---|---|---|
| **OPT101** | On (exact: the proven reports) | `x = x + [e]`, `x = x + [e1, *it]` (a list display on the right) → `x.append(e)` / `x.extend([...])` | `unconditional` | **Applied (when `safe`)** | Safety: `Unique(x)`, `ExclusiveAccessAtWrite(x)` (every alias and derived reference dead), `ExactType(x / rhs, list)`, `ExceptionStateSafe`, `EvalOrderPreserved` / Profit: `LoopCarried(x)` |
| **OPT102** | On (exact: the proven reports) | `s = s \| {e}`, `d = {**d, k: v}`, `d = {**d, **other}` → `s \|= {e}`, `d[k] = v`, `d.update(other)` | `unconditional` | **Applied (when `safe`)** | Safety: as above (dict views and iterators dead; **the accumulated dict `d` comes first: `{**other, **d}` would reverse the override order**, `DictUnpackLeading`; **the set is only tested afterwards, with `in`, `len`, truthiness, or `==`, since growing it in place can change its iteration order**, `SetOrderUnobserved`) / Profit: `LoopCarried` |
| **OPT103** | On (exact: the proven reports over a loop the data sizes) | Quadratic accumulation of immutable values (`tuple +=`, `bytes +=`, `self.s +=`, ...) → a buffer and one final conversion | `size-dependent` | Suggestion only | Safety: `Unique`, `ExclusiveAccessAtWrite`, `ExactType(lhs / rhs)`, no intermediate reads (`NoIntermediateReads`), `ExceptionStateSafe` / Profit: `LoopCarried`, `NotConstSmall` (local `s += x` excluded: CPython already appends in place) |
| **OPT110** | On (exact: the proven reports over a loop the data sizes) | `e in lst` in a loop → a `set` built before the loop | `size-dependent` | With `--include-size-dependent` | Safety: `ValueLoopInvariant(lst)`, `ExactType(lst, list \| tuple)`, hash-consistent builtin elements (`HashConsistentProbe`), `ZeroTripExceptionSafe` / Profit: `NotConstSmall(lst)` |
| **OPT111** | On (exact: the proven reports over a loop the data sizes) | `if v not in seen: seen.append(v)` in a loop → an auxiliary `set` | `size-dependent` | With `--include-size-dependent` | Safety: `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, list)`, hash-consistent builtin elements, no removals (`OnlyPairedAppends`) / Profit: `NotConstSmall` |
| **OPT112** | Off (heuristic) | `list.index` / `list.remove` / `list.count` in a loop ($O(n^2)$ scans) → a `dict` / `set` index built before the loop | `size-dependent` | None (Advisory-lane suggestion) | The receiver is certainly a builtin `list` (literal, `list(...)`, PEP 484 annotation) that lives across iterations of an `Unbounded` loop; small constant lists (`NotConstSmall`) and calls that always leave the loop are excluded |
| **OPT113** | On (exact: the reports whose premises all hold) | Nested-loop equi-join (`for a in A: for b in B: if a.k == b.k:`, $O(n \cdot m)$) → index `B` in a `dict` ($O(n + m)$) | `size-dependent` | None (Advisory-lane suggestion) | Both loops `Unbounded`; the inner iterable does not depend on the outer element; an equality between values derived from the outer and the inner element; one report per inner loop |
| **OPT120** / **OPT121** | On (exact: the proven reports over a loop the data sizes) | `q.pop(0)` / `q.insert(0, v)` / `del q[0]` / `q = q[1:]` → `collections.deque` | `size-dependent` | With `--include-size-dependent` | Safety: `Unique(q)`, no escape, only operations `deque` supports equally (`DequeEquivalentOps`) / Profit: `NotConstSmall`, **no index `q[i]` other than the ends (`0`, `-1`)** |
| **OPT130** | On (exact: the proven reports over a loop the data sizes) | Containers rebuilt in a loop although invariant (`e in set(keys)`, ...) → hoisted before the loop | `size-dependent` | With `--include-size-dependent` | Safety: every argument `ValueLoopInvariant`, no side effects (`SideEffectFree`), `ZeroTripExceptionSafe`, no changes / Profit: `NotConstSmall` |

*(`OPT112` / `OPT113` are Advisory-lane diagnostics in `heki_rules` without a rewrite; they are
not part of `heki_soundness` (ADR-004).)*

### Status of the v1.x candidates (Stage 0-A Wave 2: 339 repositories, 19 million lines, reviewed by hand)
- **Implemented, on by default once exact (heuristic today)**: `MUT107` (attributes `__hash__`
  depends on: 21.2% of mature projects; Wave 2 audit, 2026-11: about 83% of reports right about
  the shape, about 95% expected with the lazy-initialization exclusion), `OPT112` (list lookups
  in loops: 52.5% of all repositories, 81.4% of mature ones, restricted to certain `list` types),
  `OPT113` (nested-loop equi-joins: 10.2% of mature projects; Wave 2 audit, 2026-11: about 90% of
  reports right about the shape).
- **Implemented, opt-in (narrow and strict)**: `EFF106` (await or HTTP while a type-certain
  synchronous `threading.Lock` or a database `.begin()` is held: 2.1%, fatal deadlocks when it
  happens), `MUT106` (single-pass iterators that SSA does not rebind, consumed twice), `EFF107`
  (tasks bound to a local variable that does not escape; the discarded case duplicates Ruff's
  `RUF006` and is excluded).
- **Implemented, contracts**: `MUT203` (read ownership) and `EFF201` (effect boundaries), plus
  `exempt-methods` and `message` for `MUT201`.
- **Rejected by Wave 2**: `MUT104` for module `ALL_CAPS` constants (100% were deliberate
  registries or caches; mutation of `@lru_cache` results was folded into `MUT202`), `EFF108`
  (unclosed files, connections, and cursors across functions and exception paths: duplicates
  Ruff's `SIM115` and is noisy), and `MUT105` (very sparse under SSA).

### Next candidates (each must pass the gate in [docs/scope.md](docs/scope.md) and a corpus measurement first)
- **`OPT131`**: hoisting loop-invariant heavy pure calls (beyond `json.loads` / `sorted` /
  `re.compile`).
- **Mutation while iterating, across calls**: a helper called from a loop changes the collection
  the loop iterates. Ruff's `B909` covers the same-function case; heki would report only what
  needs the call graph.
- **Mutation after freezing or publishing**: an object changed after it was hashed, stored in a
  set or a cache, or handed to a read-only consumer (generalizes `MUT107` and `MUT202`).
- **Free-threading races**: shared mutable state reached from several threads without a lock,
  for free-threaded CPython builds (PEP 703).

---

## 7. Verification and quality assurance

1. **Bidirectional comparison with CPython `symtable`** (self-check of `heki_semantic`): following
   datamodel-code-generator's `_source_model.py:verify()`, heki's scope tree, bindings,
   `global` / `nonlocal`, free variables, comprehensions (3.12+ PEP 709), `__annotate__` (3.14+),
   and `lazy import` (3.15+ PEP 810) are compared both ways with the real `symtable` of CPython
   3.11–3.15 on the syntax fixtures and a corpus (zero disagreements).
2. **Adversarial differential tests and obligation-stripping tests**: every family-3 rule runs on
   boundary cases and adversarial objects on real CPython (`tests/difftest/counterexamples.py`,
   `opt_rewrites.py`, `soundness_e2e_counterexamples.py`), and removing any one safety obligation
   must make a counterexample reappear.
3. **Layer 1 sentinel verification**: sentinel objects with weak references, change detectors,
   and custom special methods check that the knowledge base (`retained_in_return` included)
   matches real CPython (`tests/sentinel/verify_soundness_kb.py`).
4. **Regression tests and dogfooding for families 1 and 2**: the edge cases from Stage 0-A and
   datamodel-code-generator (`in_thread` wrappers, sync-over-async, two-phase initialization,
   memo dicts, disjoint dict keys, `for.iter`, awaited calls, `subprocess.Popen`, DDL, the
   `DataModel.fields` write boundary with its 13 run-time witnesses, the shared
   `SchemaGraphBuilder.schema`) are end-to-end tests, and datamodel-code-generator is checked
   continuously.
5. **Coverage**: every reachable line is covered by tests; CI fails on any uncovered line except
   three parser-rejected match arms (`docs/development.md`).
6. **Benchmarks by input size (planned)**: continuous checks of the profitability classes at
   $n = 0, 1, 2, 5, 10, 30, 100, \dots$

---

## 8. System architecture (Rust workspace and incremental LSP)

```text
heki/
├── Cargo.toml                  # Rust 2024 workspace (unsafe_code = "forbid")
├── pyproject.toml              # maturin (bindings = "bin"), one binary on PyPI
├── AGENTS.md                   # rules for agents
├── crates/
│   ├── heki_ast/               # ruff_python_parser / ruff_python_ast (tag = "0.17.5", PEP 810), isolated
│   ├── heki_env/               # pyproject.toml / heki.toml configuration, requires-python, RuntimeProfile
│   ├── heki_semantic/          # scopes, bindings, module resolution, PEP 709/649/810, light class hierarchy (CHA)
│   ├── heki_ir/                # SSA-CFG IR: exception edges, evaluation order, for.iter split, MakeIter, await, LoopBound
│   ├── heki_kb/                # Layer 1 (Soundness KB) and Layer 2 (ecosystem catalog), embedded and projected at start-up
│   ├── heki_soundness/         # Soundness lane (never depends on heki_effect, checked at compile time)
│   ├── heki_effect/            # Advisory lane: call graph, EffectSummary, incremental ProjectIndex (Early Cutoff)
│   ├── heki_rules/             # family 1 (MUT1xx / MUT2xx), family 2 (EFF / DATA), family 3 (OPT), rule docs
│   ├── heki_profile/           # optional: reads profile dumps (collapsed stacks, Tachyon --gecko)
│   ├── heki_diagnostics/       # full / concise / json / github / sarif, suppressions, line-number-independent Ratchet baseline
│   ├── heki_lsp/               # incremental LSP server (tower-lsp): related call chains, code actions
│   └── heki/                   # CLI (check / server [lsp] / rule [explain] / snapshot / assumptions)
├── kb/
│   ├── soundness/              # Layer 1: native specifications for the Soundness lane only
│   └── ecosystem/              # Layer 2: effects, async safety, and offloading of the standard library and major packages
├── docs/                       # user guide (Zensical site), rules reference, llms.txt, internal design docs
├── scripts/                    # documentation generators (rules reference, CLI reference, llms.txt)
└── tests/
    ├── symtable_verify/        # heki_semantic vs the CPython symtable, both ways
    ├── difftest/               # adversarial differential tests (S1–S28 and {**other, **d})
    ├── sentinel/               # Layer 1 knowledge base sentinels
    ├── semantic/               # family 1 and family 2 regression and noise fixtures
    └── corpus/                 # syntax tour files for the frontend
```

### Crate dependency invariants (architecture invariant)
- `heki_soundness` **never depends** on `heki_effect`.
- The only knowledge `heki_soundness` may use is `EffectiveSoundnessKb` (Layer 1) and
  `RuntimeProfile`.
- The on-demand analysis of `heki_effect` (Layer 3, planned) never enters the standard library or
  packages registered in Layer 2.
- Direct dependencies on `ruff_python_*` stay inside `heki_ast`.

### Incremental LSP design (two stages for 10–50 ms responses)
- **Start-up (in the background)**: every file is analyzed in parallel; the parsed files (IR,
  semantic model, text) are dropped once every file is summarized, before propagation, and only
  the `FileSummary`s and the reverse call graph (`ProjectIndex`) stay in memory. What each
  file's check read of the declarations is kept as interned key ids. A source-level guard test
  fails when the resident state holds IR or the semantic model.
- **While editing (`didChange`, 250 ms debounce)**: only the edited file is parsed and analyzed
  again; rules within one function and cross-function rules that use the cached summaries of
  other files report immediately.
- **Propagation (`didSave`, or when a summary changes)**: when the `EffectSummary`s before and
  after the edit are equal (most edits), **Early Cutoff** skips every other file; otherwise the
  callers on the reverse call graph are re-evaluated. The files that read a changed declaration
  are re-checked: each file's summary and check record what they read of the declarations
  (modules looked up, subclass lists, property names, class facts). So are the files of
  functions whose callers changed their calls (their contract reports depend on their callers),
  the class families of changed methods, the readers of the cached properties whose stale writes
  (`MUT111`) a re-analyzed file changed, the `MUT108` / `MUT110` reports above a function whose
  own in-place changes or calls changed (their call chains walk the functions' own summaries and
  calls down to the first body that makes the change), and the diagnostics showing a call site
  that only moved; those positions are re-propagated into the callers' summaries without changing
  facts. Files changed on disk and configuration changes are followed.
  `crates/heki/tests/lsp_oracle.rs` compares the server with `heki check` after every step of an
  editing session. Each group of steps is a test of its own, a session that starts from the
  project's initial files, so the groups run in parallel: 334 steps in 40 groups, 44 steps in
  13 groups over file, layout, source root, buffer, and baseline changes, and 25 steps in five
  small projects of their own (one below a Git working tree without a configuration file).
  Measured on datamodel-code-generator: p90 of about 45 ms per file edit.
- **LSP features**: the call chains of `EFF101` / `MUT201` and others, and each diagnostic's
  help, are `DiagnosticRelatedInformation`; `OPT101` / `OPT102` are quick fixes and the proven
  size-dependent rewrites are `refactor.rewrite.heki` code actions; a code action inserts
  `# heki: ignore[CODE]`. Any editor connects with `heki server` (alias `heki lsp`).

---

## 9. CLI, configuration, Ratchet baseline, and profile dumps (one interface with Ruff and `t-linter`)

### 9.1 Commands (exit codes: `0` clean or only warnings / `1` errors, and warnings with `--fail-on warning` / `2` configuration, I/O, or syntax errors)
```bash
heki check [PATHS...]                         # semantic diagnostics (MUT, EFF / DATA, OPT)
heki check --fix [PATHS...]                   # apply safe and unconditional fixes (OPT101 / OPT102)
heki check --fix --include-size-dependent     # also the proven size-dependent rewrites
heki check --diff [PATHS...]                  # preview the fixes
heki check --select EFF,MUT --ignore MUT101   # rule selection (codes, prefixes, or names)
heki check --heuristic                        # also the heuristic rules (lint.heuristic = true)
heki check --fail-on warning                  # warnings fail the run too (lint.fail-on = "warning")
heki check --output-format full|concise|json|github|sarif   # default: HEKI_OUTPUT_FORMAT, github in GitHub Actions, full
heki check --color auto|always|never          # colored full / concise / --diff output (NO_COLOR, FORCE_COLOR, as in Ruff)
heki check --baseline heki-baseline.toml      # Ratchet: more findings fail, fewer ask to shrink the file
heki check --update-baseline                  # record a new baseline or shrink an existing one (growth is refused)
heki check --profile dump.collapsed [PATHS]   # optional: rank fixes with a recorded profile dump
heki check --watch [PATHS...]                 # re-check on file changes

heki server                                   # incremental LSP server (alias: heki lsp)
heki rule <RULE_ID>                           # the rule's documentation (alias: heki explain; like `ruff rule`)
heki snapshot summary --write [PATHS...]      # save the inferred summaries (MUT103)
heki assumptions [PATHS...]                   # list the A1 violations of a project
heki inspect <SYMBOL> [PATHS...]              # experimental: what heki knows about one function
heki recommend [PATHS...]                     # experimental: advice on the configuration (writes nothing)
```

### 9.2 Configuration (`[tool.heki]` in `pyproject.toml`, or `heki.toml`; kebab-case)
```toml
[tool.heki]
target-version = "py312"             # default: inferred from project.requires-python
extend-exclude = ["generated"]       # .gitignore and .hekiignore are honored
src = ["src", "lib"]                 # source roots (default: src/, python/, <project-name>/ as ty detects them, and workspace members)
baseline = "heki-baseline.toml"      # optional Ratchet baseline

[tool.heki.lint]
select = ["EFF", "DATA", "MUT", "OPT"]
extend-select = ["EFF105"]
ignore = []
heuristic = true                     # prefixes and the defaults also select the heuristic rules
fail-on = "warning"                  # warnings fail heki check too (default: "error")

[tool.heki.lint.severity]
MUT101 = "warning"

[tool.heki.lint.per-file-ignores]
"tests/**" = ["EFF101", "EFF102", "EFF103"]

[[tool.heki.write-owners]]
name = "generation-store"
targets = ["my_pkg.model.base.DataModel.fields"]
allowed-writers = ["my_pkg.parser.generation.*"]
exempt-methods = ["__deepcopy__", "copy_deep"]        # construction and copy methods
message = "use GenerationStore.set_fields() instead"  # appended to every report

[[tool.heki.read-owners]]                             # MUT203, the read side of MUT201
name = "reference-children"
targets = ["my_pkg.reference.Reference.children"]
allowed-readers = ["*GenerationIndex*", "*GenerationStore*"]

[[tool.heki.readonly-returns]]
name = "shared-schema"
producers = ["my_pkg.parser._schema_graph.SchemaGraphBuilder.schema"]
deep = true

[[tool.heki.effect-boundaries]]                       # EFF201
name = "pure-render"
scopes = ["*.render", "*.render_module_code", "my_pkg.domain.*"]
forbid = ["writes-targets", "blocking"]               # also blocking-net/db/fs/subprocess/sync, sleep, mutates-params
write-targets = ["DataModel.*", "DataType.*"]
allowed-callees = ["my_pkg.logging.*"]
message = "render entry points must not mutate model state"
```

### 9.3 Inline suppressions (no conflict with Ruff's `RUF100`)
- **Recommended (no configuration, no conflict with Ruff)**: `# heki: ignore` /
  `# heki: ignore[EFF101]` / `# heki: ignore[async-blocking-call]` at the end of the line, the
  same syntax as `t-linter` and ty.
- **Compatible**: `# noqa: EFF101` is accepted too (for users who combine it with
  `lint.external = ["MUT", "EFF", "OPT", "DATA"]` in Ruff).
- **Opt-in checks of the comments**: `lint.report-unused-suppressions` reports the codes that
  suppressed nothing (only codes of heki rules that ran; another tool's code and a bare `# noqa`
  stay) and baseline entries that matched nothing, and `--fix` removes the codes;
  `lint.require-suppression-codes` reports a bare `# heki: ignore`. They are settings, not rules:
  the rules are the claims of the Effect & Alias theory about the code.
- **Profile dumps**: running a program under a profiler costs overhead and permissions (macOS
  needs `sudo` for Tachyon and `py-spy`), so it is never required. heki **reads dumps that were
  already recorded** as an optional add-on: collapsed stacks (Tachyon `--collapsed`, `py-spy`)
  and Tachyon `--gecko` JSON.

---

## 10. Development history and quantitative gates

### Stage 0-A: large-scale measurement gate (completed 2026-11-19)
- **Result**: 118 mature open-source repositories (12.42 million lines) and 221 new GitHub
  repositories of 2026 (5.68 million lines), **339 repositories, 56,629 files, 18.1 million
  lines**, were scanned and all 171 samples of the 8 rules were reviewed by hand; datamodel-code-
  generator's 20,000 lines of in-house checkers were analyzed.
- **Decisions**: `EFF101` / `EFF102` / `EFF103` / `DATA101` adopted as the core, `MUT101` adopted
  with strict noise exclusions, `MUT102` / `DATA102` rejected by measurement; from the
  datamodel-code-generator study, **`MUT201` / `MUT202`, the Ratchet baseline, and the
  `symtable` comparison** were added; import boundaries are left to tach and Python 3.15 PEP 810.

### Implementation milestones (completed)
| Milestone | Delivered |
|---|---|
| M1 | `heki_semantic`: scopes and bindings for Python 3.11–3.15, zero `symtable` disagreements on the fixtures and on datamodel-code-generator's 422 files |
| M2 | `heki_ir`: SSA-CFG construction (Braun et al.), `for.iter` split, `LoopBound`, `LoopCarried`, A1 hazards, `heki assumptions` |
| M3 | `heki_kb`: embedded Layer 1 / Layer 2 TOML with `deny_unknown_fields`, sentinel verification on CPython 3.11–3.15 |
| M4 | `heki_effect`: call resolution, Tarjan SCC fixpoint of `EffectSummary`, Early Cutoff, offload wrappers, sync-over-async, boundary-only reporting |
| M5 | `MUT101` / `MUT201` / `MUT202` / `EFF101`–`EFF103` / `EFF105` / `DATA101`, the CLI, output formats, suppressions, and the Ratchet baseline (datamodel-code-generator checked in 1.72 s) |
| M6 | `heki_soundness`, the `OPT` rules, `--fix` / `--diff` / `--profile`, and `MUT103` snapshots; every counterexample blocked or disproved, zero unsafe rewrites in CPython differential tests |
| M7 | The incremental LSP server and `--watch` |
| M8 | End-to-end test suite through the real binary and LSP process; 100% of reachable lines |
| M9 | CI / release / documentation workflows and the user guide |
| v1.x | `MUT106`, `MUT107`, `MUT203`, `EFF106`, `EFF107`, `EFF201`, `OPT112`, `OPT113`, `MUT108`, `MUT109`, `MUT110`, `MUT204`, the `EFF201` extensions, the `MUT201` extensions, the `MUT202` return flow, `MUT205`, `MUT111`, and contract keys, entries, and fresh objects (31 rules) |
| v0.2.0 | `MUT206`, `MUT207`, `MUT112`, `MUT113`, string constants bound once and class constants as keys (35 rules) |
| v0.2.1 | `EFF109` and `EFF110`: a non-reentrant lock taken again while it is held, and an object allowing one operation at a time used by tasks started together |
| v0.2.4 | `EFF112`: the iteration order of a set of strings reaching an output across a call |
| v0.2.4 | `EFF114`: a `RecursionError` a broad `except` around a recursion turns into a value |
| v0.2.5 | `EFF115`: an `async def` handed to a library call that never awaits what it returns |

### Gates for every release (from Stage 0-B)
1. **Semantic consistency**: zero disagreements with the CPython `symtable` on scopes and bindings.
2. **Zero unsafe rewrites**: no `safe` rewrite changes behavior (manual review and differential
   tests; every counterexample S1–S28 and `{**other, **d}` passes).
3. **Rule certainty** ([ADR-018](docs/DESIGN_HISTORY_AND_ADR.md#adr-018)): an exact rule builds
   its reports from established premises only (section 3.4), and is certified by:
   (a) premise-stripping tests: for each premise, a counterexample in the tests that the rule
   reports once that premise is taken as established, and that it does not report otherwise;
   (b) a census of every report it has on the development corpus, each checked by hand, with
   every false claim fixed as a defect of a premise's fact, never by a list of exclusions;
   (c) one run on the frozen holdout corpus (100 packages never looked at while the rule was
   designed or fixed) with no false claim outside a violation of R1–R7; and (d) the run-time
   oracle (`sys.monitoring`) on the differential test suite and on datamodel-code-generator,
   finding every claimed event where the code ran. Every report a release adds or changes in an
   exact rule is checked. A false claim found later is a bug, fixed before the next release, or
   the rule becomes heuristic in that release. The shape-right and real-harm rates of every
   rule are published in [docs/precision.md](docs/precision.md); real harm decides only whether
   an exact rule runs by default.
   - Published as a measure of value, not a gate: **at least one convincing finding in 30% or
     more** of the target projects (the contract rules `MUT201` / `MUT202` show at least the
     coverage of datamodel-code-generator's own checker on its code).
4. **Performance**: a 100,000-line repository is analyzed in a few seconds.
5. **Release integrity**: the release workflow runs CI on the release commit and checks the tag
   against every distribution's version before it publishes.
6. **Memory**: CI (`scripts/check_memory.py`) bounds the peak heap of `heki check`, with and
   without an ownership contract, and of the `--watch` engine on a generated project, counted by
   a counting build that reads the same bytes on every run; at release time, the peak memory as
   the system sees it (resident set size on Linux, physical footprint on macOS) under a coarse
   budget.

### Remaining for v1.0
The conservative `SoundnessSummary` (A2), Layer 3 on-demand analysis with its disk cache,
`OPT131` and the other candidates above after measurement, benchmarks by input size, at least
100,000 adversarial differential cases per rewrite rule on each CPython version from 3.12 to
3.15, and checks on major open-source projects (datamodel-code-generator included).
