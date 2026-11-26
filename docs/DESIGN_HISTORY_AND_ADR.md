# Design History, Revision Log, and Architecture Decision Records (ADRs)

> **What this document is**
> The supplement to [ARCHITECTURE.md](../ARCHITECTURE.md): how the design arrived at its current
> form, the evidence behind it (large-scale measurements: 19.7 million lines plus the Stage 0-A
> Wave 1/2 corpora of 19 million lines, counterexample experiments, a study of
> datamodel-code-generator's 196,000 lines and its 20,000 lines of in-house checkers, and checks
> of CPython 3.15 PEP 810 and `ruff_python_parser` 0.17.5), the reasons for every revision from
> v1.0 to v4.1, and every major decision (ADR-001 to ADR-023). The project's working name was
> `pyopt`; crate names below use the current `heki_*` names.

---

## 1. Starting point and the owner's stance

### 1.1 The original idea
The first plan was a performance optimizer that brought Rust's ownership and borrowing to Python
static analysis by combining (1) inference without annotations (Stage 1), (2) an explicit
contract API (Stage 2: `Owned` / `Borrowed` / `move()` / `@pure`), and (3) formal verification in
Lean 4.

### 1.2 The goal and framing that discussion and measurement settled on
- Rust ownership and Lean 4 are **means, not ends**.
- **No ad-hoc fixes and no deferring**: one complete architecture based on one theory, a tool of
  industry-standard quality that runs in CI without overlapping Ruff or ty.
- After large corpus measurements (more than 37 million lines in total) and a deep study of
  datamodel-code-generator (196,000 lines plus 20,000 lines of in-house defensive
  infrastructure), the framing converged on: **an Effect (execution and state change) and Alias
  (uniqueness and ownership boundaries) semantic checker for Python, the third static analysis
  layer after Ruff and ty, with speedups as a natural by-product**
  ([ADR-012](#adr-012) to [ADR-016](#adr-016)).

---

## 2. Facts established by measurements, primary sources, and real projects

### 2.1 Large corpus (19.7 million lines), counterexample experiments (S1–S28), and CPython 3.15
1. **Pure-Python container rewrites alone are rare, and default automatic fixes even rarer**:
   - About 130 container optimizations per million lines look provable without annotations
     (one per about 7,600 lines).
   - Small-size measurements (A.8) show that only **`OPT101` and `OPT102`** are faster from
     $n = 1$ on (`unconditional`): about 20 provable cases in 13.2 million lines of mature code,
     1 in 6.5 million lines of new code. `OPT103` is 0.23–0.84 times as fast for small loops, and
     `OPT130` 0.01 times as fast (about 100 times slower) for loops that run zero times.
2. **Uniqueness (frame locality) holds, but derived references and the invariance of object
   contents must be tracked (S1–S28)**:
   - Experiments confirmed that `for v in x: x = x + [v]` becomes an infinite loop when rewritten
     (S23), that rewrites break while `iter` / `enumerate` / `zip` / `map` / generator expressions
     / `d.keys()` are alive (S24–S26b), and that changes from outside or through aliases during a
     loop break them too (S27–S28).
3. **Constant-factor micro-optimizations often backfire, and propagating effects up from the
   lowest primitives misfires on `asyncio`**:
   - Caching builtins and bound methods in locals is 2–12% slower on CPython 3.15, and
     `re.compile` only gains a constant factor because of its internal cache.
   - A `sys.setprofile` measurement (A.7) showed that correct async code,
     `asyncio.StreamWriter.write(data)`, reaches the non-blocking `socket.send` in two hops.

### 2.2 Stage 0-A measurement (339 repositories / 56,629 files / 18,108,143 lines, all 171 samples reviewed by hand)
- **Corpus**: 118 mature open-source repositories (12.42 million lines) and 221 new GitHub
  repositories of 2026 (5.68 million lines), scanned with 8 workers (62.8 s); 171 samples of 8
  rules reviewed by hand. These figures describe that analysis; each rule's certainty and its
  current rates are in [docs/precision.md](precision.md).
- **Adopted as the core**:
  - `EFF101`: **37.6%** of web/async repositories (82.3% indirect), **68% real harm** (including
    a real `time.sleep` in redis-py `redis/asyncio/cluster.py:1613` reached in 4–5 hops). The
    need to exclude awaited calls and `subprocess.Popen` was identified.
  - `EFF102`: **44.1%** of web/async and **48.5%** of database repositories (90–93% indirect);
    about 75–80% real harm after excluding `for.iter` (NVIDIA NeMo, OmniAgent, luigi, ...).
  - `EFF103`: **47.6%** of database repositories (74.2% indirect). The need to exclude
    `for.iter`, DDL, migrations, and ORM internals was identified.
  - `DATA101`: **19.5%** of data/ML repositories. The 60% false positives of a naive AST check
    were all non-`LoopCarried` padding; with the SSA `LoopCarried` check (Stage 0-A audit,
    2026-11) about 90% of reports were right about the shape.
- **Made strict or rejected**:
  - `MUT101`: **16.1%** of mature repositories (real hits in datamodel-code-generator, mkdocs,
    wagtail, textual, ...). The three noise sources of the naive version (two-phase
    initialization, memo dicts, disjoint dict keys) and the separation of nested-class
    `__init__` were identified; the strict version was adopted.
  - `MUT102a` / `MUT102b`: 4.2% of mature and 0.5–1.4% of new repositories, sparse and mostly
    deliberate idioms: **rejected by measurement**.
  - `DATA102` (`df.iterrows()`): duplicates single-expression lints: **rejected**.

### 2.3 A real production project: datamodel-code-generator (196,000 lines plus 20,000 lines of in-house checkers)
1. **Performance, CLI start-up, and native code**:
   - The pure core (6 JSON schemas, `Formatter.BUILTIN`: 1,771 ms) scaled exactly linearly,
     $O(N)$, in every scaling test. 45% of a medium CLI run (269 ms) was start-up and the import
     of 395 modules, so turning parts into native extensions (`.so`) would be counterproductive
     for a CLI (checked by the owner and by measurement).
   - The default `OPT101` / `OPT102` fixes apply 0 times (and `_target_binding.py:465` gave a new
     counterexample: `{**other, **found}` accumulates on the right). `_runtime/client/` gave two
     findings: the `await in_thread(self.open)` wrapper propagation and the sync-over-async
     (`run_sync`) relaxation.
   - A scan for `@cached_property` **hit a real bug of datamodel-code-generator 0.86.0
     (`jsonschema.py:1460`) on the first try**.
2. **What the in-house defensive infrastructure (about 20,272 lines) taught**:
   - The project maintains **9,762 lines** of static checkers in `scripts/`, **4,685 lines** of
     run-time monitoring in `tests/checks/`, and **5,825 lines** of unit tests for the checkers.
   - Its foundation, `scripts/_source_model.py` (2,433 lines), states itself that *value flow,
     container aliases, attribute types, and call graphs cannot be answered from the AST*. As a
     result `check_generation_store_usage.py` (1,665 lines) tracks local aliases by hand and
     still **misses 13 cases that depend on run-time `__setattr__` monitoring (`witnesses`)**,
     and `tests/checks/schema_graph.py` (236 lines) has to compare deep fingerprints to verify
     that the shared `JsonSchemaObject` is not changed.
   - All of this generalizes to **state effects, aliases, and the call graph** in `heki_effect`
     (**`MUT201` write ownership** and **`MUT202` read-only returns**). The line-number-independent,
     shrink-only baseline (`check_ratchets.py` with `generation_store_allowlist.toml`) and the
     bidirectional comparison with CPython `symtable` (`_source_model.py:verify()`) are worth
     adopting as core techniques of the tool's design and quality assurance.
3. **Checks on CPython 3.15 (Tachyon and PEP 810 `lazy import`)**:
   - Tachyon (`profiling.sampling`) cannot combine `--opcodes` with `--jsonl`, and needs `sudo`
     on macOS. Profile integration is limited to reading recorded dumps (optional).
   - **PEP 810 (`lazy import json` / `lazy from pathlib import Path` → `is_lazy=1`) is in CPython
     3.15.0, checked on the real interpreter**. Lazy imports and import cycles are left to tach
     and Python 3.15+ PEP 810, and heki stays with effects and aliases (until ADR-022).

---

## 3. Evolution of the design (v1.0 → v2.0 → v2.1 → v3.0 → v4.0)

### 3.1 Summary of v1.0 → v3.0
- **v1.0 → v2.0**: the two lanes physically separated (Soundness vs Effect), the three-layer
  knowledge model, Datalog rejected, the Safety × Profitability model, the `LoopBound` gate,
  Stage 0-A first.
- **v2.0 → v2.1**: derived references (`derived_refs`), `ValueLoopInvariant`, an independent
  `SoundnessSummary` inside the Soundness lane, `OPT103` / `OPT130` made size-dependent, a pure
  Layer 1 and no Layer 3 dives into the standard library, staged `EFF101` with boundary
  aggregation, `EFF104` rejected.
- **v2.1 → v3.0**: after measuring datamodel-code-generator's 196,000 lines, the product was
  redefined as an "Effect & Alias semantic checker (speedups are a natural by-product)"; native
  extensions and ad-hoc rules became non-goals; Tachyon became optional dumps; offload wrapper
  propagation and the sync-over-async relaxation were added; `OPT102` was limited to a leading
  accumulator.

### 3.2 Baseline v3.0 → Baseline v4.0 (Stage 0-A done, in-house checker study, PEP 810 checked)

| Topic | Baseline v3.0 | What measurement and study showed | Baseline v4.0 decision | Related ADR |
|---|---|---|---|---|
| **Stage 0-A rule decisions** | `MUT101` / `102`, `EFF101`–`103`, `DATA101` / `102` to be measured | In 339 repositories and 171 samples, `EFF101`–`103` / `DATA101` are dense and harmful, `MUT102` is sparse and noisy, `DATA102` duplicates Ruff | Adopt `EFF101`–`103`, `DATA101` (SSA `LoopCarried` required), and `MUT101` (strict); **formally reject `MUT102` / `DATA102`**; list the next candidates (`MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131`) | [ADR-014](#adr-014) |
| **Protecting state in mature code (declared boundaries)** | Inference only (`MUT101` / `102` / `103`) | datamodel-code-generator protects "write boundaries" and "immutability of shared objects" with 20,000 lines of in-house checkers and run-time monitoring, and hits the limits of the AST (13 misses) | Adopt **`MUT201` (write ownership)**, **`MUT202` (read-only returns)**, the **line-number-independent Ratchet baseline**, and the **`symtable` comparison** | [ADR-015](#adr-015) |
| **Import boundaries and Python 3.15 PEP 810** | Not specified | tach / import-linter exist, and CPython 3.15.0 ships **PEP 810 (`lazy import`, `is_lazy=1`)**, checked on the real interpreter | General import-layer and import-cycle checks are **non-goals left to other tools**; heki stays with effects and aliases; `heki_ast` / `heki_semantic` support the PEP 810 syntax and lazy-evaluation semantics | [ADR-016](#adr-016), [ADR-022](#adr-022) |

Baseline v4.1 (ADR-017) fixed the name `heki`, the interface shared with Ruff and `t-linter`, the
incremental LSP design, and the Stage 0-A Wave 2 decisions. ADR-022 wrote heki's scope as five
points and took imports into it as walls: import facts with the time each import runs, rules
that need no configuration, the import contracts of other tools, and heki's own import contracts.
ADR-023 ordered `heki recommend`'s advice by where an invariant is best stated: a standard type
first, then a convention the code follows, then a contract.

---

## 4. Architecture Decision Records (ADRs)

<a id="adr-001"></a>
### ADR-001: The product's axis — an independent middle end, not a judge for Ruff's unsafe fixes
- **Decision**: no judge for Ruff's unsafe fixes (`pyopt judge`); an independent middle-end
  analyzer instead (merged into the "Effect & Alias semantic checker" by [ADR-012](#adr-012)).
- **Reason**: most of Ruff's unsafe fixes can be decided with type information and risk being
  absorbed by ty, and tracking upstream would cost too much. Effects across functions (74–93% of
  the findings are indirect) and alias / mutation analysis are structurally outside Ruff.

<a id="adr-002"></a>
### ADR-002: The custom contract API (Stage 2: `Owned` / `Borrowed` / `move()` / `@pure`) is rejected
- **Decision**: no custom contract API in user code (a non-goal). Instead: **standard `typing`,
  zero-config inference, boundaries declared in `pyproject.toml` (`MUT201` / `MUT202`), and
  `heki snapshot summary`**.
- **Reason**: to avoid function coloring, package dependencies, run-time overhead, and barriers to
  adoption.

<a id="adr-003"></a>
### ADR-003: Lean 4 is rejected; rigor comes from axiomatic analysis and adversarial differential tests
- **Decision**: no Lean 4 (a non-goal). Instead: conservative abstract interpretation (a
  lattice), declarative three-valued proof obligations, adversarial differential tests (with
  obligation-stripping tests), and Layer 1 sentinel verification.

<a id="adr-004"></a>
### ADR-004: Two lanes physically separated on one IR (`heki_soundness` vs `heki_effect`), each with its own summaries
- **Decision**: the analyses below the SSA-CFG IR are split into the crates `heki_soundness` and
  `heki_effect`, and `heki_soundness` depending on `heki_effect` is a compile-time error.
  - `heki_soundness`: never trusts type annotations; uses only `ExactType`, `Unique`,
    `derived_refs`, and `ValueLoopInvariant`. For A2 it is to get its own conservative
    `SoundnessSummary` (planned).
  - `heki_effect`: light receiver type resolution (name resolution, PEP 484, `self` attributes,
    CHA / RTA) computing one `EffectSummary` (execution effects, mutation and state effects, and
    write ownership). It continues on functions that violate A1.
  - A fix's text may rewrite an annotation (`list[int]` to `deque[int]` for a `deque` fix):
    the Soundness lane rewrites the annotation's text and never trusts it as a type.

<a id="adr-005"></a>
### ADR-005: The three-layer knowledge model (a pure Layer 1, one Layer 2, no Layer 3 dives, a full dependency cache)
- **Decision**: Layer 1 (Soundness only, no effects, sentinel verification, demotion past
  `verified_max_python`), Layer 2 (public standard-library APIs and major packages: the origin of
  every effect, `async_safe`, offloading, `logging` excluded, copy sanitizers), and Layer 3
  (on-demand analysis of unregistered pure-Python dependencies only, never the standard library
  or Layer 2 packages, with a cache keyed by `effective_kb_digest` and `env_packages_digest`),
  projected once at start-up.
- **Status**: Layers 1 and 2 are implemented; Layer 3 and its cache are planned. Until then,
  functions outside the checked files that Layer 2 does not list have no known effects. The
  Advisory lane reads Layer 1's `read_only` and `retained_in_return` entries for the parameters
  a function only reads; Layer 1 still holds no effect.

<a id="adr-006"></a>
### ADR-006: Embedded Datalog (`ascent`) is rejected in favor of a dedicated engine
- **Decision**: no embedded Datalog. Instead: an RPO worklist abstract interpretation over a
  provenance-carrying lattice, declarative three-valued proof obligations, and a Tarjan SCC
  worklist fixpoint.

<a id="adr-007"></a>
### ADR-007: Safety × Profitability, the `LoopBound` gate, and honest expectations about automatic fixes
- **Decision**: safety (behavior preservation) and profitability (`LoopCarried`, `NotConstSmall`,
  dependence on input size) are separate. `unconditional` (applied by the default `--fix`) is
  only `OPT101` and `OPT102`; `OPT103` and `OPT110`–`OPT130` are `size-dependent`; `OPT120` /
  `OPT121` forbid any index `q[i]` other than the ends.
- **Amended in 0.2.0**: the default `--fix` rewrites `s = s | t` to `s |= t` only where nothing
  observes the set's iteration order (`SetOrderUnobserved`): growing a set in place and copying
  it into a new one can order the same elements differently, and the order of an `int` set is the
  same in every run.
- **Amended after 0.4.0**: a fix may also be unsafe (`RuleMeta::unsafe_fix`): one that changes
  what the program does in a way the rule's page states, applied only with `--unsafe-fixes`
  (Ruff's flag) and offered by the language server as a quick fix that is not preferred. The
  first is `MUT118`'s: an `object()` sentinel becomes `sentinel("NAME")` (PEP 661) for a Python
  3.15 target, which copies keep, while its type, repr, and pickling change. *Rejected*: applying
  it with `--include-size-dependent`, whose fixes keep what the program does.

<a id="adr-008"></a>
### ADR-008: Advisory precision (staging, boundary aggregation, offload wrapper propagation, sync-over-async relaxation, `EFF104` rejected)
- **Decision**: `EFF101` is limited to high-impact blocking; `BlockingFs` is split into the
  opt-in `EFF105`; `logging` / `print` are excluded. Boundary-only reporting with root-cause
  grouping, offload wrapper propagation, the sync-over-async relaxation, `EFF104` rejected, and
  ORM lazy loading stated as out of scope for `EFF103`.

<a id="adr-009"></a>
### ADR-009: Minimal, isolated use of Ruff crates; scopes, CFG, and SSA IR built in-house
- **Decision**: only `ruff_python_parser` / `ruff_python_ast` are used, pinned exactly (a version
  with PEP 810), and isolated in `heki_ast`; scope resolution, the CFG, and the SSA IR are built
  in-house.

<a id="adr-010"></a>
### ADR-010: Stage 0-A measurement first, with strict quantitative gates
- **Decision**: every core rule and every candidate rule is judged by a corpus measurement and a
  manual review before it is adopted. The Stage 0-B gates are zero `symtable` disagreements,
  zero unsafe rewrites, a strict Advisory false-positive rate below 5%, practical noise below
  20%, a convincing finding in at least 30% of the target projects (shown on
  datamodel-code-generator for the contract rules), and 100,000 lines analyzed in a few seconds.
- **Superseded in part by [ADR-018](#adr-018)**: the false-positive and noise gates give way to
  rule certainty (a rule runs by default only when its reports are built from established
  premises and its certification found no false claim), and the 30% measure is published as a
  measure of value, not a gate.

<a id="adr-011"></a>
### ADR-011: Derived references, invariance of object contents, and the unpacking order of `OPT102`
- **Decision**: `FrameState` gets `derived_refs` with transitive escape, `ExclusiveAccessAtWrite`,
  merges of up to 4 `alloc_id`s, and `ValueLoopInvariant` (against S23–S28). `OPT102` rewrites a
  dict unpacking only when the accumulated dict comes first (`{**d, ...}`) (against
  datamodel-code-generator `_target_binding.py:465`).

<a id="adr-012"></a>
### ADR-012: Product redefinition — an "Effect & Alias semantic checker (speedups are a by-product)"
- **Decision**: the product is **"an Effect & Alias semantic checker for Python (the third static
  analysis layer after Ruff and ty)"**, and **"speedups follow as a natural by-product"**. Family
  1 (`MUT`), family 2 (`EFF` / `DATA`), and family 3 (`OPT`) all derive from one SSA-CFG and
  call-graph foundation.

<a id="adr-013"></a>
### ADR-013: Native extensions (AOT / mypyc) and ad-hoc framework optimizations excluded; external dumps optional
- **Decision**: ahead-of-time native extensions, build-time rewriting, and framework-specific
  heuristics are non-goals; profile integration is limited to reading offline dumps, without
  touching the program (an optional add-on).
- **Amended in 0.3.0**: the optional add-on is removed (`heki check --profile` and the
  `heki_profile` crate). A profile could only choose among the proven size-dependent rewrites the
  run shows: on the development corpus (378 packages, 30.75 million lines) the default run of 0.2.6
  had 3 such reports, and a run of every rule with `--heuristic` 85. Since 0.2.4 a proven rewrite
  over a loop whose trip count the data does not decide is heuristic and not shown, so
  `--fix --profile` skipped it even in a function on the stack in every sample: the README's own
  `drain` example fixed nothing. The reader had no known user and nothing was built on it after
  0.1.0, and `--include-size-dependent` applies the same rewrites in every function. heki still
  never runs or instruments the program.

<a id="adr-014"></a>
### ADR-014: Rule decisions from the Stage 0-A measurement (`MUT102` / `DATA102` rejected, SSA required for `DATA101`, strict `MUT101`)
- **Background**: 339 repositories (18.1 million lines) and all 171 samples reviewed by hand.
- **Decision**:
  1. Adopt `EFF101` / `EFF102` / `EFF103` / `DATA101` as the core; in the IR, keep `for.iter`
     outside the loop, distinguish `is_awaited`, exclude `subprocess.Popen` and DDL, relax
     `DaemonPolling`, and require SSA `LoopCarried` for `DATA101`.
  2. Adopt `MUT101` with its four required filters: two-phase initialization hooks, memo dicts,
     disjoint constant dict keys, and the separation of nested classes.
  3. **Formally reject `MUT102` (deep mutation, representation exposure) and `DATA102`
     (`df.iterrows()`) based on the measurements.**
  4. Keep the further candidates (`MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131`) for
     v1.x and put them through the same measurement gate before implementation (done for Wave 2
     in [ADR-017](#adr-017)).

<a id="adr-015"></a>
### ADR-015: Declared write ownership (`MUT201`), read-only returns (`MUT202`), the Ratchet baseline, and the `symtable` comparison
- **Background**: the study of datamodel-code-generator's in-house defensive infrastructure (20,272
  lines) showed that the biggest pain of mature production code, which neither Ruff nor ty can
  protect and which forces in-house AST checkers and run-time monitoring, is "model state written
  outside the designated store (through aliases, container operations, and indirect calls)" and
  "corruption of shared cached objects". It also showed that large-scale adoption needs a
  line-number-independent, shrink-only baseline, and that the semantic analyzer's reliability
  needs a comparison with CPython's `symtable`.
- **Decision**:
  1. **`MUT201` (write ownership)** and **`MUT202` (read-only returns)**, declared in
     `pyproject.toml`, become core members of family 1 (they ride on the existing state effect
     and alias engine unchanged). v1.x added **`MUT203` (read ownership)**, `exempt-methods`, and
     `message`.
  2. The Ratchet baseline (line-number-independent keys `(path, qualname, rule, target) = count`,
     shrink only) becomes a standard feature: `heki check --baseline` / `--update-baseline`.
  3. The quality assurance of `heki_semantic` requires the **bidirectional comparison with the
     CPython `symtable`** (CPython 3.11–3.15 in CI).

<a id="adr-016"></a>
### ADR-016: No import-boundary or import-cycle checks (left to tach); semantic support for Python 3.15 PEP 810 (`lazy import`) (decision 1 superseded by ADR-022)
- **Background**: datamodel-code-generator has `_import_cycles.py` (117 lines),
  `check_architecture_boundaries.py` (1,977 lines), and `redundant-local-import`, while tach /
  import-linter already exist, and CPython 3.15.0 was confirmed to ship **PEP 810 (`lazy import`
  / `lazy from ... import ...`, `is_lazy=1`)**.
- **Decision**:
  1. General import-layer boundaries and import-cycle checks stay out of heki's core and are left
     to tach / import-linter and PEP 810: **heki stays with effects and aliases**.
  2. `heki_ast` and `heki_semantic` **fully support the syntax of Python 3.15 PEP 810
     (`is_lazy=1`) and its semantics of materializing on first use** (a requirement for the pinned
     `ruff_python_parser` version).
- **Superseded in part by [ADR-022](#adr-022)**: decision 1 gives way to import checks in heki
  (the `IMP` family); decision 2 stands.

<a id="adr-017"></a>
### ADR-017: The name `heki`, one interface with Ruff and `t-linter`, the incremental LSP design, and the Stage 0-A Wave 2 decisions
- **Background**: before implementation, the project needed (1) a name short enough for
  `# <name>: ignore[EFF101]` at the end of a line, easy to pronounce in Japanese and English (no
  `v` / `th`), searchable, and free on PyPI and crates.io; (2) a CLI and configuration that sit
  naturally next to Ruff and `t-linter`; (3) realistic LSP latency; and (4) the advance
  measurement of the nine v1.x candidate rules.
- **Decision**:
  1. **The working name `heki` (Japanese for "wall")**: four letters (as long as `ruff`), no `v` /
     `th`, no clash with religious terms, slang, or IT companies in search, and free on PyPI
     (HTTP 404) and crates.io (HTTP 404), checked. (The first candidate `kabe` was dropped because
     it collides with the Turkish word for the Kaaba. The final public name was still being
     considered at the time.) The name stands for every rule's theme: a barrier around write
     ownership, shared immutability, crossings into `async`, and alias boundaries.
  2. **One interface with Ruff and `t-linter`**: one Rust binary distributed with `maturin`; the
     CLI `heki check` / `heki check --fix` / `heki server` (alias `lsp`) / `heki rule` (alias
     `explain`) / `heki snapshot` / `heki assumptions`; exit codes `0 / 1 / 2`; configuration in
     `pyproject.toml` `[tool.heki]` (kebab-case), `heki.toml`, and `.hekiignore`; inline
     suppressions `# heki: ignore[...]` first (no conflict with Ruff's `RUF100`), `# noqa:`
     accepted too.
  3. **Incremental LSP design (Early Cutoff)**: from day one, `heki_effect::ProjectIndex` keeps no
     IR, only the light `FileSummary`s and the reverse call graph; `didChange` (250 ms debounce)
     re-evaluates one file, and `didSave` or a changed summary re-evaluates only the affected
     callers through Early Cutoff.
  4. **Stage 0-A Wave 2 (339 repositories, 19 million lines, 30.5 s, with a manual audit)**:
     - **First v1.x rules**: `MUT107` (attributes `__hash__` depends on: 21.2% of mature
       projects; Wave 2 audit, 2026-11: about 83% of reports right about the shape), `OPT112`
       (`list.index` / `remove` in loops: 81.4% of mature projects, restricted by type), `OPT113`
       (nested-loop equi-joins: 10.2% of mature projects; Wave 2 audit, 2026-11: about 90% of
       reports right about the shape).
     - **Adopted when narrow and strict (v1.x opt-in)**: `EFF106` (`await` / HTTP while a
       type-certain synchronous lock or a database `.begin()` is held), `MUT106` (single-pass
       iterators that SSA does not rebind, consumed twice), `EFF107` (tasks bound to a local
       variable that does not escape; the discarded case duplicates Ruff's `RUF006` and is
       excluded).
     - **Rejected by Wave 2**: `MUT104` for module `ALL_CAPS` constants (100% were deliberate
       registries or caches; mutation of `@lru_cache` results was folded into `MUT202`),
       `EFF108` (duplicates Ruff's `SIM115` and is noisy), `MUT105` (very sparse under SSA).
- **Status**: all of the above is implemented (the v1.x rules on 2026-11-20).

<a id="adr-018"></a>
### ADR-018: Exact and heuristic rules
- **Background**: the third precision review (2026-11-21) hand-checked up to 50 reports of every
  rule that ran by default. Every rule had reports wrong about the code, and none had 100 checked
  reports; the old gate (strict false positives below 5%) let rules with known wrong reports run
  by default. The goal is that every rule running by default is right about what it reports.
  The cause is the polarity of "unknown": `docs/scope.md` promised "no report, never a guess",
  which the code kept where a callee does not resolve and nowhere else. For a branch, the
  repetition of a call in a loop, a size, the meaning of an attribute write, a library effect
  with a condition, or a dispatch to several overrides, unknown led to a report, and each fix
  round added recognizers of harmless shapes to an open list. By the third review's ranking,
  94% of the estimated false reports came from two such premises: "the effect runs on this
  path, every time" and "the loop is data-sized". A review of the design by three independent
  analyses agreed on this cause and on the decisions below.
- **Decision**:
  1. **The claim** (D1): a report states what its message says about the code, in the terms its
     rule page defines under "What a report states"; it is wrong when that statement is false.
     Harm and usefulness (small by domain, deliberate, a memo heki does not recognize, a blocked
     rewrite) are not part of it. *Rejected*: every documented exclusion as part of the claim
     (each exclusion heki cannot recognize in every form becomes a permanent wrong-report class,
     and documenting less is rewarded); harm-based precision (harm and shape are separate
     questions).
  2. **A claim is a conjunction of premises, each established or open.** A premise is a fact of
     one home: a call resolves; a function reaches the next call on every path on which it
     returns; a statement runs on every iteration that completes; a loop steps through a named
     input; a library call has its effect under its condition; a store is a plain store; two
     names hold the same object. A premise fact answers with the bounded walks' `Answer`
     (`Known(A)` / `Open`), with no second type for "unknown". An **exact** report is built from
     established premises only: `Open`, a budget that runs out, or a fact heki lacks stops it. A
     **heuristic** report may rest on an open premise, and its rule page names which. A false
     exact report is a defect of one premise's fact, fixed there with a counterexample, never by
     an exclusion list in the rule. *Rejected*: recognizing more harmless shapes (the list does
     not converge: round 3 found 36 groups where round 2 had 13, and 62 of round 2's 100 wrong
     samples still failed after five fix pull requests).
  3. **The claim is anchored at the report.** "Whenever the reported call returns, the effect
     has run": the first hop is the reported call, so a branch around it in its own function
     does not matter, and each later function must reach the next call on every path on which
     it returns. *Rejected*: requiring the whole `async def` to reach the effect on every path
     (it keeps 17% of `EFF101`'s reports instead of 37%, and 1 of its 16 harmful samples
     instead of 5).
  4. **Size is never claimed by an exact report** (replaces the earlier D2). A bound of 16 or
     fewer that the code states only removes reports; a larger fixed trip count is silent, and an
     exact loop claim names the input the loop steps through (a parameter, an attribute of one,
     the rows of a query, the lines of a file). A heuristic report may still rest on
     "data-sized", the code not fixing the size at 16 or fewer, which its rule page states in
     plain words. *Rejected*: "data-sized" as part of the exact claim (a complete size evaluator
     does not exist, so every bound heki cannot read would be a false report); reporting a fixed
     count with its count ("on each of 50 iterations"), true but not worth the line.
  5. **Assumptions R1–R7** ([ARCHITECTURE.md §3.4](../ARCHITECTURE.md)): static bindings,
     annotations (R2), the catalog, imports as configured, the checked files as the project
     (R5: a receiver typed by a `Protocol` stays unresolved, as its implementers are never a
     complete set), and a library call's argument as a waiting value unless heki establishes
     otherwise (R6: a catalog entry that waits lists the values for which it returns at once),
     and no asynchronous or resource-exhaustion exception (R7: `MemoryError`,
     `KeyboardInterrupt`, a signal; without it nothing runs "whenever" anything else does).
     An exact claim holds in every run consistent with them; "a branch heki cannot decide can go
     both ways" is not one of them. A context manager heki does not resolve may suppress
     exceptions: exact facts do not pass through its body after an instruction that may raise,
     a choice of silence rather than an assumption. R2 is for the reporting rules only: the Soundness lane and
     `--fix` never rely on annotations (ADR-004, A4). R2 grants an instance of `C` or of a
     project subclass, so a premise through a method call holds only when every candidate
     implementation has it: an override in a project subclass, of a project class or of a
     library class, leaves it open. For a nominal container ABC (`Sequence`, `Mapping`,
     `collections.abc.Set`, their mutable ABCs, which define no `__subclasshook__`, so an
     instance derives from the ABC or is registered with it), R2 grants a builtin of its family
     or another library class implementing it, taken to run what the builtin runs, as a
     `dict`-typed value that is a `defaultdict` is taken to, or a project implementer: a class
     whose bases reach, through project classes, the ABC or a class of the family table (the
     builtins and the standard-library classes CPython registers with the ABC or derives from
     them, checked on CPython 3.11 to 3.15), or a class registered with one. A `register` heki
     cannot resolve, or a `__subclasshook__` of such a class, opens the family; a structural ABC
     (`Iterable`, `Collection`) or a protocol stays open, and a base outside the project and the
     table stands for itself. *Rejected*: reading every abstract class as open (a function that
     only iterates its `Sequence` gets no exact fact); following the implementers' overrides as
     call targets (the effect rules' heuristic tiers stay as they are, as for a `dict`-typed
     receiver). A type fact keeps its provenance (constructed here, or an
     annotation under R2), so that reports resting on R2 can be dropped where annotations are
     not trusted.
  6. **Rewrite validity is fix safety** (D3): an `OPT` report's cost claim (what runs in the
     loop) does not include whether the rewrite is valid; the help names the obligation that
     blocks it. A proven rewrite is a premise of the rule's exact tier (decision 9).
     *Rejected*: silence until `ValueLoopInvariant` and `HashConsistentProbe` are established
     for every report (at least 19% of `OPT110`).
  7. **Messages**: an exact report's message states its claim and the witnesses of its
     premises only (the chain's calls, the input a loop steps through); a cost estimate such as
     "(N+1)" and advice such as "batch the queries" leave it (advice goes into the help). Where
     the candidates of a dispatch reach different effects, the message names each per candidate.
  8. **Names** (D4): `exact` and `heuristic`, on an axis called `certainty`; only heuristic
     reports are labelled in output, as Ruff labels preview rules. *Rejected*: `proven` (a
     report needs evidence, a rewrite a proof), `precise`, `certain`, CodeQL's four
     `@precision` levels, Pylint's per-message confidence, and Semgrep's three confidence
     levels (heki's bar is binary).
  9. **Where the class lives** (D5): `RuleMeta::certainty`, and `activation` (`Default`,
     `OptIn`, `Contract`) instead of `default_enabled`. A report's certainty is its rule's at
     most (`Certainty::at_most`). One claim is one code: an effect rule's heuristic side is a
     code of its own (`EFF101` "reaches on every path" beside `EFF111` "can reach", `EFF103`
     beside `EFF113`). An `OPT` rule keeps one code with a certainty per report: a report whose
     rewrite is proven safe, in a loop whose trip count the code does not fix, is the rule's
     exact tier (for `OPT113`, a join whose rule page premises all hold), the others are
     heuristic. A demoted rule
     keeps `Default`, so `lint.heuristic = true` restores the earlier default set and a
     promotion is a one-field change. Experimental stays its own axis and implies `OptIn`.
     *Rejected*: a second list of heuristic codes; one code with a per-report tier for the
     effect rules (their claims differ, and a rule is its claim); a selection that reads a
     per-report certainty (the selection stays per rule, and a filter withholds the heuristic
     reports of an exact rule).
     *Amended in 0.4.0*: the effect rules keep one code too, with a certainty per report:
     `EFF111` is now the heuristic tier of `EFF101`, and `EFF113` that of `EFF103`. Every gain in
     precision moved a report from the heuristic code to the exact one, so it changed the
     report's code: an inline suppression stopped matching it, a baseline counted it as new, and
     the entry of the old code was left unused. 13 rules already carried both tiers under one
     code (the eight `OPT` rewrite rules, `OPT113`, `DATA101`, `MUT110`, `MUT114`, and
     `MUT119`), `DATA101` split by the same must-facts as the effect rules, and within one code a
     baseline entry already holds across a change of tier. The heuristic tier states the
     weaker claim of the same premises, each open premise recorded as for the other rules. The
     old codes are no aliases, as `MUT109` was not in 0.3.0: in a selector, a suppression, or a
     baseline entry they are read like any unknown code. Decision 10 is unchanged: naming
     `EFF101` or `EFF103` shows its heuristic reports.
     *Amended by baseline version 3*: a baseline entry no longer holds across every change of
     tier. An entry allows the claims it recorded and no stronger one: it records how many of its
     sites were heuristic (`heuristic`), and a run regresses when it finds more sites than the
     entry's `count`, or more exact sites than `count` less `heuristic`. An exact report that
     turns heuristic passes, and `--update-baseline` shrinks the entry to it; a recorded
     heuristic report that is exact now fails once, also after an upgrade in which heki proves
     it, until it is fixed or the baseline is recorded anew. Both tiers of a rule key a report
     alike, so that a change of tier stays within its entry: `EFF101` and `EFF103` key by the
     last call of the shortest call chain, also where the exact message names the call every
     implementation runs. A baseline of version 2 is compared as before, with a note, until
     `--update-baseline` rewrites it; an older heki refuses version 3 instead of ignoring the
     count. *Rejected*: an exception for the reports an upgrade proves (it would reopen the hole
     at every upgrade); keying the exact report by the call it claims (adding a branch on the way
     would turn its entry into a new one).
  10. **Selecting the heuristic rules and reports** (D6): `[tool.heki.lint] heuristic = true` and
      `--heuristic` / `--no-heuristic` (on `check` and `server`), modelled on Ruff's `preview`:
      prefixes and `ALL` then also select the heuristic `Default` rules, and an exact code or name
      selects any rule either way. The heuristic reports of an exact rule are shown where a
      heuristic rule would run: with the key, when its code or name is selected, or for a
      contract rule. They are withheld after inline suppressions and before baselines, fixes, and
      folding, and a text run ends with a note counting the rules left off and the reports
      withheld (Ruff's note for the fixes it holds back). *Rejected*: a pseudo-selector
      (`extend-select = ["HEURISTIC"]`); requiring the key even for an exact code (it would
      break `extend-select = ["EFF105"]`).
  11. **Contract rules** (D7): classified and labelled like the others; declaring the contract
      selects them, whatever their certainty. `MUT202`'s zero-configuration part (`@lru_cache` /
      `@cache` producers) is a code of its own, `MUT114`, a `Default` rule whose reports are
      exact when the changed object is the list, dict, or set a cached function returned, the
      same object on every path; `MUT202` keeps the declared producers. *Rejected*: exempting contract rules from the class (`EFF201`
      calls itself over-approximate); gating them on `heuristic` (a declared contract would
      report nothing).
  12. **Certification** (replaces the earlier D8 and D9): a rule becomes exact when (a) for
      each premise a counterexample in the tests is reported once that premise is taken as
      established, and not otherwise (premise stripping); (b) a census checks by hand every
      report the rule has on the development corpus, each false claim fixed as a premise
      defect, never by an exclusion list; (c) one run on a frozen holdout corpus of 100
      packages, never looked at during design and fixes, finds no false claim outside a
      violation of R1–R7; and (d) a run-time oracle (`sys.monitoring`) on the differential test
      suite and on datamodel-code-generator finds every claimed event where the code ran.
      Every report a release adds or changes in an exact rule is checked; a confirmed false
      claim blocks the release until it is fixed or the rule is demoted in that release. The
      certifications are in [docs/precision.md](precision.md), and `build_docs.py` fails for an
      exact rule without one. *Rejected*: a sample of 100 reports with no wrong one as the bar
      (it bounds the rate below 3%, but cannot establish the contract, and the wrong-report
      classes cluster by package); a rate threshold (wrong reports below 5%); an exception for
      `OPT101` and `OPT102` (they are certified like the others, so until then `heki check
      --fix` applies no fix without configuration).
      *Amended by [ADR-023](#adr-023)*: a suggestion kind of `heki recommend` is certified on a
      sample, as a kind has thousands of instances (about 11,300 read-only parameter candidates
      on 41 packages of the development corpus). Two reviewers read 300 groups from at least 30
      packages, since false claims cluster by package, and find no false claim, which bounds the
      rate below 1%; a sample of 100 groups from the holdout corpus is read once; the run-time
      oracle copies each suggested parameter's container at entry and compares it at exit, on
      the differential test suite and on datamodel-code-generator; and the mechanical checks run
      on every group: the result parses, it equals the original apart from annotations and the
      added import, the annotation trial and the corpus gate's verification run find no change, and with
      every exact suggestion applied `MUT110` reports nothing. A false claim found later is a
      premise defect, fixed as for a rule. A sample is enough here, where it is not for a rule,
      because the mechanical checks cover every group and a suggestion is read before it is
      applied: a false one costs a reviewed edit, not a false report in every run.
      *Recorded for the values of container ABCs (decision 5)*: the cases they newly establish
      in `MUT101` and `MUT111` have a counterexample each in `tests/difftest/container_abcs/`,
      run on CPython, that heki reports heuristic (an implementer through the ABC, through
      `UserDict`, and through `list`, a registered class, a `register` heki cannot resolve, a
      `__subclasshook__`, a structural ABC); `lsp_oracle.rs` follows another file's `Mapping`
      implementer and `register` call; the census of every report the change adds or changes on
      the corpus and the holdout run are read before the release that ships it.
  13. **The default set**: a rule runs by default when it is exact and its share of real harm
      is worth the line it prints. For 0.2.0 that is `EFF101`, `EFF103`, `OPT113`, and the
      proven tier of the `OPT` rules once each is certified; `MUT107`, `OPT112`, and `EFF102`
      stay heuristic. *Rejected*: every exact rule by default (a rule whose reports are right but rarely
      harmful, such as `MUT107` with 4% real harm in round 3, would print lines nobody acts
      on); releasing 0.2.0 with the earlier default set.
      *Amended in 0.3.0*: the six size-dependent `OPT` rules (`OPT103`, `OPT110`, `OPT111`,
      `OPT120`, `OPT121`, `OPT130`) are opt-in, by this decision's own criterion read on their
      exact tier: of their exact reports, 2 of the 203 the census classified and 1 of the 233 of
      the holdout run were of real harm, and a run of the 0.3.0 build on the holdout has 126,
      all benign ([docs/precision.md](precision.md#harm-of-the-exact-reports)). A benign report
      of theirs is also a rewrite that does not pay (ADR-007). `OPT101` and `OPT102` stay in the
      default set, as the one exception: their rewrite is faster for every input and the
      default `--fix` applies it. `OPT113` stays on its own share (2 of 8 census reports).
      `--include-size-dependent` selects the five rules whose rewrites it applies, as if
      `extend-select` named them. *Rejected*: keeping them on with an exception for proven
      rewrites (the published bar and the published default set would disagree).
      *Amended on 2026-11-27*: a rule with fewer than 10 exact reports in its census and
      holdout run runs by default on at least two real bugs it found, with no false claim and
      a run-time oracle that shows both sides on generated programs, instead of on its share
      of real harm ([docs/precision.md](precision.md#rare-failures)). Rare write-time shapes,
      such as an import that raises or a lock that waits for itself, are fixed before code
      matures, so they never reach 100 reports in the corpus, and below 10 one report moves the
      share by more than 10 points. The certifications already made on 1 to 8 reports
      (`OPT102`, `OPT130`, `DATA101`, `MUT110`, `MUT114`, `OPT111`, `OPT110`) had no stated
      rule for it. `MUT110` and `MUT114` stay in the default set on their recorded census, and
      `build_docs.py` exempts rules of consequence `MinorBug` and the rules certified before
      this amendment. *Rejected*: adding packages to the census for such a rule (a source scan
      of 143 packages found no report for a shape mature code no longer has).
- **Status**: the certainty and activation of every rule, a report's own certainty (the `OPT`
  reports whose rewrite is not proven are heuristic), the `heuristic` key and options, the
  withholding of an exact rule's heuristic reports, the note, the labels in every output, the
  "What a report states" section of every rule page, and the certification check are
  implemented. The proven tier of the `OPT` rewrite rules and the exact tier of `OPT113` are
  certified in 0.2.0; the other rules are heuristic until certified. The measurements are in
  [docs/precision.md](precision.md), and release gate 3 of
  [ARCHITECTURE.md](../ARCHITECTURE.md) is the certification.


<a id="adr-019"></a>
### ADR-019: Severity by consequence
- **Background**: the default severity followed the rule family: `warning` for `OPT`, `error`
  for `MUT`, `EFF`, and `DATA`. A blocking call on the event loop, a query in a loop, and a
  quadratic concatenation make code slower, as the `OPT` rules' findings do, yet were errors.
- **Decision**: `RuleMeta::consequence` says what happens in the code a rule reports, and sets
  its default severity: `Bug` (a wrong result, an exception, a lost update, a deadlock, or a
  broken contract) is `error`; `MinorBug` (a broken promise that callers seldom notice, such as
  `MUT110`'s read-only annotation changed by a helper, with no harmful report among those
  checked) and `Slowdown` are `warning`. `EFF101`, `EFF102`, `EFF103`, `EFF105`,
  `DATA101`, and the `OPT` rules are slowdowns; `EFF106` (a synchronous lock or a transaction
  held across I/O can deadlock the event loop), `EFF107`, `EFF201`, and every `MUT` rule are
  bugs. A rule split off another (a heuristic sibling code) has its parent's consequence.
  `lint.severity` still overrides per rule.
  *Rejected*: severity by family (it says how a rule is built, not what the code does).
- **Exit code**: only an `error` report makes `heki check` fail (exit code 1). A `warning` is
  printed, and shown by the language server, without failing the run, unless
  `lint.fail-on = "warning"` (`--fail-on warning`) asks for it. A check that fails on every
  slower loop, a report users may judge harmless in their domain, stands in their way and is
  turned off, and then its bug reports go unseen too; a bug is worth stopping for. With a
  baseline the same threshold applies to the Ratchet regressions. A refused `--update-baseline`
  (the update did not happen) and pending `--diff` changes (files would change) still exit 1.
  *Rejected*: failing on both severities (the 0.1 behavior); an `--exit-zero` switch (it hides
  the bugs too).
- **Amended in 0.4.0**: only a certain bug that can be critical fails a run.
  1. `EFF101` is a `Bug`, an `error`: a call that blocks the event loop holds up every
     concurrent task while it waits, as `EFF106`'s deadlock does. Its heuristic reports (a
     call that can reach a blocking call) keep the severity and fail nothing by default (point 2).
     Slower code (`EFF102`, `EFF103`, `EFF105`, `DATA101`, the `OPT` rules) stays a
     `warning`: of the 4 `EFF103` reports on an event-loop boundary in the corpus, none adds a
     package the `EFF101` reports do not already fail.
  2. A heuristic report counts as a `warning` for `fail-on`, whatever its severity
     (`Diagnostic::fails_run`). It can be wrong about the code, and a check that fails on a
     report that may be false is turned off as surely as one that fails on every slower loop.
     The report keeps its rule's severity in every output: the severity says what the bug costs
     if the report is right, the certainty whether it is. `fail-on = "warning"` fails on it,
     which is how a project makes its contracts, whose rules are heuristic until certified, stop
     a run.

  On the development corpus (364 packages analyzed), a run with no configuration fails 36
  packages instead of 7 (the median newly failing package has 2 reports, the largest 13), with
  the same reports. *Rejected*: failing on every exact report, slower code included (a new
  `fail-on` value as the default: 62 packages fail); `fail-on = "warning"` as the default, as
  Ruff and ty do (62, and 249 with every rule and `--heuristic`, mostly on reports heki labels as
  possibly wrong); showing a heuristic report of an `error` rule as a `warning` (the label and the
  SARIF level would stop saying what the bug costs).
### ADR-020: `heki recommend`: advice on the configuration from the project's own analysis
- **Background**: what to enable and declare is the user's question after a first run: which
  heuristic reports could be made exact, which opt-in rules would report here, which ownership
  and effect contracts the code already keeps. Answers from names or guesses would be advice
  heki cannot stand behind, and a false catalog entry would make exact reports false (R3).
- **Decision**: an experimental subcommand, `heki recommend [PATHS]`, registered as
  `command.recommend`. It takes `heki check`'s analysis options, which define the configuration
  the advice is relative to, analyzes the project with `heki check`'s pipeline, and prints
  sections with counts, reasons, and caveats, a `[tool.heki]` block, and a JSON object with a
  published JSON Schema generated from the types that write it. Its analysis lives in
  `heki_rules::recommend`, so the language server can use it later. Exit codes: `0`, or `2`
  for configuration, I/O, or syntax errors; never `1`. The advice is kept honest by these
  rules:
  1. Every count is computed from this project by the pipeline `heki check` runs (inline
     suppressions, `per-file-ignores`, `relax-async-in-tests`, the baseline).
  2. A contract enters the block only when a trial run of its rules, with the strict sibling,
     reports no violation of it on the code as it is and hides no report of the configuration's
     own contracts; reports of the strict sibling make it medium, with `unresolved = "ignore"`.
     Kept contracts whose entries differ only in their targets or scopes are one entry, and a
     pattern naming every attribute of a class enters it only after a trial of its own reports
     nothing the targets it stands for did not. A run with the whole block then applies it as
     `heki check` would.
  3. A catalog entry enters the block only when heki can verify every key it sets. heki cannot
     verify a library's effects, so library entries never enter it; such calls are listed as
     questions, with what a trial entry would report. `src` enters only after its trial.
  4. Annotations and code changes are never in the block. Annotations are suggestions of their
     own, with edits and a verification of their own ([ADR-023](#adr-023)).
  5. Likely truth quotes only published measurements, with their sample, interval, and date,
     or the open premises of these reports; what is not measured is said so. No score is
     synthesized.
  6. Confidence levels are measurable conditions.
  7. A hypothetical is printed as a conditional and kept out of the block and out of every
     count of reports.
  8. Paths narrower than the project root give a note, and lower the confidence of contracts.
     *Replaced by [ADR-023](#adr-023) decision 12*: a path selects what is shown, and the
     analysis covers the project.
  9. The same input gives the same report: caps are counts, never times, and output is sorted.
  10. It writes nothing, runs nothing, and keeps every value the configuration sets.
  11. A cause the facts do not record is shown as not recorded, never guessed.
  *Rejected*: a `heki check --recommend` flag (it would collide with `check`'s modes and exit
  codes); catalog entries guessed from names or call shapes (rule 3); a probability or score
  per rule (rule 5).
### ADR-021: Attribute types of library classes in the catalog
- **Background**: the LLM SDKs make their calls through attributes of a client
  (`client.chat.completions.create(...)`), and the receiver typing read an attribute's type only
  from project declarations, so a catalog entry for `Completions.create` was reached only through
  an annotated `Completions`. The catalog described calls only.
- **Decision**: an `[[attributes]]` table types a read of an attribute on an instance of a library
  class (`openai.OpenAI.chat` gives `openai.resources.chat.Chat`), consulted only where the
  project declares nothing for the name and the row is established (the classes the value may be
  under R2 find the row first, with no binder, `__getattribute__`, unknown metaclass, or unknown
  base on the way): anywhere else the read stays untyped, in both tiers, since types carry no
  provenance. A builder returning its receiver's class is `return_type = "Self"`. Rows are
  checked against the releases of their distribution (`[[distributions]]`, `verified`), and the
  analysis never reads a project's versions, so its output does not depend on the environment.
  Projects add rows with `attribute-type`.
  *Rejected*: entries keyed by access path (`openai.OpenAI.chat.completions.create`: they do
  not unify with an annotated `Completions` nor join over two clients); a `__getattr__`
  catch-all (that the installed release has no real member of the name cannot be established);
  version facts in the engine from lock files and specifiers (a later step, if the verified
  ranges prove too narrow); reading the installed packages or their stubs (ADR-005, and output
  that would depend on the environment).

<a id="adr-022"></a>
### ADR-022: heki's scope, and imports as walls: import facts, rules without configuration, other tools' contracts, and heki contracts (supersedes ADR-016 decision 1)
- **Background**: heki's scope was written as non-goals and gate questions, and one of them
  named a tool's domain instead of a kind of check: import layering and import cycles were left
  to tach and import-linter (ADR-016). The production project of §2.3 still keeps its own layer
  check and cycle script, because those tools count every import alike: they do not know which
  imports run while a module loads, which run later or lazily (PEP 810), which never run
  (`TYPE_CHECKING`), or which modules a constant name given to `importlib.import_module` loads.
  Whether a cycle fails depends on the order of statements in module bodies, which needs control
  flow. A second tool also means a second configuration, baseline, suppression syntax, and
  language server.
- **Decision**:
  1. heki's scope has five points, written in `docs/scope.md` ("What heki covers") and
     summarized in `AGENTS.md`:
     - the core: what a program does across functions and modules (effects, changes, aliasing,
       purity, async and resource behavior), which one file or one type cannot decide;
     - walls: contracts the user writes or heki proposes on who may write, read, call, reach an
       effect, and import what;
     - out of scope: single-file patterns and style (Ruff, isort) and type correctness (type
       checkers);
     - ideas from other languages are taken when they fit the core or the walls and pass the
       evidence gate: counts on a corpus, real harm, and no false exact claim;
     - framework knowledge enters as catalog data; rules written for one framework stay a
       non-goal.

     Of the non-goals in `AGENTS.md` 2.4, only the import one changes: which modules may import
     which, and import cycles, are walls.
  2. heki checks imports in a new family `IMP`. Decision 1 of ADR-016 is superseded; its
     decision 2 (PEP 810 support) stands.
  3. Without configuration, heki checks what the project itself declares: circular imports that
     fail, package code importing test or development code, and dependencies declared only for
     development or in an extra that a package or entry module loads while it loads. Each runs
     opt-in until a corpus review finds no false claim among its reports.
  4. Contracts the project already declared for import-linter, tach, or Ruff's import bans are
     read as heki contracts when present (`import-contracts-from`; `[]` turns it off). Each
     field heki does not support is named once in a note. From Ruff's bans heki reports only
     what Ruff cannot see.
  5. Import facts have one home, `heki_effect::imports`: which module each import site loads,
     its timing class (`body`, `lazy`, `function`, `type-checking`), and the failure its
     handlers take, one ordered fact: none, a missing module (`ModuleNotFoundError`), or any
     import error (`ImportError`, `Exception`, a bare `except`). Each rule asks for the failure
     it claims: `IMP102` for any import error, since a name a partly loaded module lacks raises
     a plain `ImportError`; `IMP104`, `IMP301`, `IMP302`, and the walk of what a module loads
     for them, for a missing module; the sets of what a module loads at least count an import
     only when no handler takes either. The choice between a submodule and an attribute lives
     in `ProjectDecls` and serves name resolution too. No rule resolves an import itself.
  6. The facts are local and IR-free, in `FileSummary`, and take part in Early Cutoff. The
     import graph is built per check and not kept in `ProjectIndex`.
  7. A report states where an import is written, its timing class, and the module it loads; it
     does not claim that a branch is taken. An open target stops an exact report; a contract's
     strict sibling (`IMP202`) reports it with `unresolved = "report"`.
  8. A cycle is reported once per strongly connected group. Loading the packages that hold the
     importing module is no edge, since Python loaded them before the module ran; loading any
     other parent package is (`import pkg.sub.leaf` runs `pkg/sub/__init__.py`).
  9. Definitions are not import targets: `call-owners` and `read-owners` keep them.
  10. Ruff and isort keep what one import statement decides: order (`I001`), required imports
      (`I002`), import conventions (`ICN`), relative style (`TID252`), and the `lazy` policy of
      one file. heki's groups follow the project's Ruff or isort classification (known lists,
      sections), and a disagreement with heki's own detection is a configuration note.
  11. Import facts are compared with CPython both ways, and imported contracts with the tools
      they come from.
- *Rejected*: leaving imports to tach and import-linter; a resolver inside the rules; one report
  per elementary cycle (their number can be exponential); the packages holding a module as
  edges of its imports (every submodule would close a cycle with its `__init__`); a rule for
  unused declared dependencies (plugins, entry points, and tools are used without an import);
  reporting the sites Ruff's own bans report; translating other tools' files into `[tool.heki]`
  tables (two declarations of one contract).

<a id="adr-023"></a>
### ADR-023: Type-first advice: `heki recommend` suggests the standard types the code already keeps
- **Background**: `heki recommend` (ADR-020) proposes contracts for what the code already keeps:
  the owners of attributes, effect boundaries, read-only returns. Many of these invariants are
  what a standard annotation says. A `list` parameter that the function and every function it is
  handed to only read is a `Sequence`; an attribute bound once while its object is constructed
  is `Final`. A contract is read by heki alone, while an annotation is read by the type checker,
  the editor, and everyone who reads the signature, and heki already follows read-only parameter
  types across calls (`MUT110`). On datamodel-code-generator `heki recommend` proposes 90
  contracts in 502 lines, and 4 of its 5 `write-owners` entries without writers state what
  `Final` states.
- **Decision**:
  1. **Triage.** `heki recommend` puts each invariant it finds in the first of these places that
     can say it, and prints its sections in this order:
     1. the code, as a standard type or marker (`Sequence`, `Mapping`, `collections.abc.Set`,
        `Final`, and later `@final`, `Literal`, and `ReadOnly`): an edit of annotations only,
        enforced by the type checker where it sees the type and by an exact heki rule where it
        does not;
     2. a convention the code already follows (private modules imported only inside their
        package, `__all__`, the dependency tables): nothing to add, since a rule without
        configuration reads it;
     3. a contract in the configuration, for what nothing in the code can say, in its shortest
        form;
     4. nowhere: an invariant nothing outside its module reads, one in test, generated, or
        vendored code, or a claim heki cannot establish.

     A fact the code can state in a standard form needs no second declaration that only heki
     reads. heki never suggests a change of what runs: no `tuple` or `frozenset` conversion,
     `frozen=True`, `__slots__`, rename, or import moved under `TYPE_CHECKING`; a proven rewrite
     stays an `OPT` fix (ADR-007). *Rejected*: contracts first (a second place for a fact the code
     can state); run-time changes as advice (they need the proof a fix needs, and a maintainer
     cannot accept them by reading a claim).
  2. **A suggestion is a claim with premises.** It states what the code keeps as a conjunction of
     premises, each established or open, as a report does (ADR-018 decision 2), and carries the
     edits that write the claim in standard typing. It is exact when every premise is
     established: `Open`, a budget that runs out, or a fact heki lacks stops it. Each premise is
     a row with one home, and the documentation lists them under "What a suggestion states". A
     false exact suggestion is a defect of one premise's fact, fixed there with a counterexample.
     When the claim holds and only a premise of the edit is open (a slice of the parameter is
     stored, the function is used as a value), the suggestion is heuristic: listed with `--all`,
     with the open premise and its place, and never in `--diff` or in the editor. When the claim
     itself is open, nothing is listed, and the open cause is counted under what heki cannot see.
     Edits that are valid only together form one group, applied whole or not at all: a parameter
     and the parameters it is handed to, and the same parameter in every definition of an
     override family. Of 40 read-only parameter candidates judged on the development corpus, 11
     failed the type check when applied alone, 5 because the value is handed to another
     annotated parameter and 4 because the method belongs to an override family.
  3. **Assumptions.** R1 to R5 hold as for exact reports ([ARCHITECTURE.md
     §3.4](../ARCHITECTURE.md)), and three more:
     - **T1, annotations stay unread**: nothing reads the function's or its class's annotations
       at run time except what heki checks for. A decorator outside the decorator table
       (`heki_effect::decorators`, which gains a column for decorators that leave annotations
       unread), class machinery the catalog does not list, and a use of the function as a value
       leave the premise open. A registry that finds functions by scanning a namespace is not
       seen; where the project has reader sites (`get_type_hints`, `inspect.signature`,
       `__annotations__`), the notes say where. *Rejected*: one project-wide flag "reads
       annotations somewhere" (on the development corpus it is set for 18 of 41 packages and
       removes half of the candidates, while a reader needs the function object, which it gets
       in one of the four ways above).
     - **T2, the checked files are the project** (R5 for edits): overriders, implementers, and
       callers outside the checked files are not known. A suggestion on a public name says so
       (decision 11).
     - **T3, a type checker runs**: half of each guard is the type checker's. heki looks for a
       checker's configuration (`[tool.ty]`, `ty.toml`, `[tool.mypy]`, `mypy.ini`,
       `[tool.pyright]`, `pyrightconfig.json`, `[tool.pyrefly]`) and says so when it finds none
       (decision 14).
  4. **The annotation trial and the verification run.** An annotation is also an input of heki's
     own analysis (R2): a `set` parameter widened to `Set` no longer has a set's iteration order,
     a slice of a `Sequence` is no longer known to copy, and a method call on a `Mapping` may
     reach a project class that implements it. Before a group is shown, heki analyzes each file
     of the groups again with their edits applied as the JSON prints them (the heads, and the
     import where the edit puts it), the files in parallel, and compares its local facts as Early
     Cutoff compares them: its functions' summaries, call sites, declarations, and opt-in tables,
     the places the edit moved at their offsets in the original file, apart from what restates
     the edit (the annotations' own facts, the changed parameters' read-only and mutable bits,
     the added import). It then builds the index once with those files summarized again and
     compares the edited files' reports with every rule selected by rule, certainty, severity,
     function, target,
     places, message (numbers aside: rules print lines), and the sites it counts. A difference
     withholds the groups with an edit in the function it names (the file's groups when none
     has one) and names the fact or report that changed; a withheld group leaves every file, and a file whose edits
     changed is analyzed again, at most four times, its reports checked at most twice. The
     heuristic groups come last, on top of the exact ones kept, by their facts only, so their
     "No fact lost" is never established. The trial
     keeps no summary or parse of its own, so its peak stays near the discovery run's: the index
     holds one summary per file and stays taken apart (local facts only) while files are
     analyzed, a file it holds edited is summarized again as it is when compared, and the edited
     files are parsed in batches of a thirty-second of the source and stand in for theirs, which
     are parsed again from their text after the check. The other
     files need no analysis: of an edited function's annotation another file reads only the
     union of the read-only and mutable bits (`MUT110`'s test of a callee), which a mutable head
     turned read-only keeps, so when no edited file's local facts differ, no other file's
     summary does (ADR-017); an added import a star import of the module would bind elsewhere
     leaves the group heuristic. The added import is the one fact another file's reports read
     besides those: an `import-boundaries` contract walking imports (`reach`) judges what its
     importers reach, so, as the language server re-checks them when a module's imports change,
     the trial checks the files of the modules such a contract judges, edited or not, with the
     exact groups that add an import. A report that changes there withholds the groups adding
     an import to a file of its import chain (every one it was checked with when none does),
     and a file's check stands only for the groups it was checked with: its own and, for such
     a file, those adding an import. The corpus gate applies every exact suggestion to a copy of
     each package and compares `heki check` with every rule selected by its code, before and
     after: the reports must be equal. A suggestion therefore never removes or demotes a report,
     and never creates a report of its own guard. *Rejected*: the verification run in the
     product (on the largest package it costs about as much as the rest of `recommend`, and
     shows that a report changed, not which group changed it); an analysis per group (its cost
     grows with the groups times the file); patching annotations in the IR (lowering and name
     binding would not run).
  5. **Advice, not a report or a fix.** A suggestion has no rule code, severity, `select` entry,
     suppression comment, baseline entry, or exit code. `heki check` never prints one, and
     `heki check --fix` never applies one: ADR-007 and ADR-011 judge the rewrite of a diagnostic
     on safety and profitability, and a suggestion is neither a diagnostic nor a change of what
     runs. Its safety statement is "annotation-only", which heki checks on the result before it
     prints it: the edited file parses, and it equals the original once annotations and the
     added import are removed from both syntax trees; a file that fails either check is withheld
     and named in the notes. `heki recommend` gets no option that writes files:
     `heki recommend --focus types --diff` prints the exact groups as one unified diff that
     `git apply` applies, and the command still exits with 0, or 2 when it cannot run (ADR-020).
     Once applied, a suggestion is enforced like any annotation: the type checker and heki report
     a violation in the run that reports everything else. *Rejected*: `--fix` or a writing
     option for suggestions (an edit of public signatures and of several files is the
     maintainer's to review, and `--diff` leaves applying it to the tools that already do that).
  6. **Each suggestion names its guard**, what reports a later violation of the type it writes:

     | After the edit | The type checker reports | heki reports | Gap |
     |---|---|---|---|
     | a `Sequence` / `Mapping` parameter | a change in the function; handing it to a `list` / `dict` parameter | `MUT110` (exact, on by default, a warning): handing it to a function without annotations, or annotated `Any`, that changes it, any number of calls down | none while a type checker runs |
     | a `Set` parameter | the same | `MUT110`, once it reads `collections.abc.Set` and `typing.AbstractSet`, which it does before the first `Set` suggestion | none after that |
     | a `Final` attribute | rebinding where it sees the class | nothing by default; `MUT108` (opt-in, heuristic) reports in-place changes of the container it holds | rebinding in a function that does not see the type |
     | a read-only constant, attribute, or result (`Final[Sequence[...]]` and the like) | a change through the name; handing it to a `list` parameter | `MUT110`, once its claim grows (decision 7) | none after that |
     | `@final`, `Literal` | a subclass, an override, another value | nothing; they keep heki's own premises established | none |

     `MUT110` stays a warning (ADR-019). It has no exact report in the default run of the 0.3.0
     release on the 378 packages of the development corpus, so after adoption its reports are
     regressions of annotations the maintainer accepted; the first such reports are read before
     its severity changes, and `lint.severity` makes it fail a run for a team that wants that.
  7. **`MUT108` is not promoted; `MUT110`'s claim grows.** `Final`, `frozen`, and `NamedTuple`
     stop rebinding, and the container inside is often filled on purpose (PEP 591 promises no
     more for `Final`). A review of 50 of `MUT108`'s 169 reports on the development corpus found
     49 right and none harmful, so it stays opt-in by ADR-018 decision 13. The type that says
     "not changed" is the read-only declared type, so `MUT110`'s claim grows from parameters to
     every value declared `Sequence`, `Mapping`, or `Set` (an attribute, a module or class
     constant, a call result, a property) handed as itself to a function that does not see the
     type and changes it in place. It keeps the code `MUT110`: its premises stay
     "read-only by declaration", "passed as itself", and "changed at the line", the read-only
     judgment keeps its one home (`ParamAccess` in `heki_effect::resolver`), and a baseline keeps
     holding. The wider claim passes the release gate of ADR-018 and ships before the constant
     and result kinds it guards. *Rejected*: promoting `MUT108` (right reports that are rarely
     harmful, by ADR-018 decision 13); a code of its own for the wider claim (one claim is one
     code, ADR-018 decision 9).
  8. **JSON version 3.** The JSON object becomes version 3 (`docs/schemas/recommend-3.json`,
     generated from the types as before) once, in the release that adds suggestions and merges
     contract candidates with equal owners into one entry with a list of `targets`: the new shape
     of the contract entries is a change of meaning under the versioning rule, and one change of
     version in a release is one migration for agents instead of two. A new object
     `suggestions` holds the counts per kind, the type checker heki found, the groups, and the
     groups withheld with their reasons. A group has an `id` without a line number (kind, module,
     qualified name, target, as a baseline key has), its `kind`, `certainty`, `items`, byte-span
     `edits` with a path each (a group can span files), `premises`, `open`, `related`, `guards`,
     and `labels`. Lists hold at most 50 entries and say when they hold fewer than there are;
     `--all` lifts the cap.
  9. **In the editor, a hint and an action, no diagnostic.** The server's diagnostics stay what
     `heki check` reports. A suggestion reaches the editor as an inlay hint after the annotation,
     with the claim and the guard in its tooltip, on functions edited in the session
     (`heki server --suggestions edited|all|off`, default `edited`), and as a code action
     `refactor.rewrite.heki.annotate` whose workspace edit holds every edit of the group: never a
     quick fix and never the preferred action. Groups over 8 edits or 3 files are left to the
     CLI. For every file the server's groups equal those of
     `heki recommend FILE --focus types --output-format json`, since both run the same function
     over the index and the file's IR, and `lsp_oracle.rs` checks it. The hint and the action are
     experimental, as `behavior.server-suggestions`. *Rejected*: a diagnostic of hint severity
     (it would put suggestions among reports, where agents that read diagnostics act on them,
     and the server's diagnostics would no longer equal `heki check`'s); the code action alone
     (it is found only by asking).
  10. **No note in `heki check --touched`.** It would repeat at every commit for a suggestion the
      maintainer declined, and there is no dismissal (decision 16). The editor and an agent's
      finish loop (`heki recommend --touched FILES --focus types`) are where suggestions reach
      people.
  11. **Labels.** `Sequence[str]` also admits a plain `str`, which `list[str]` rejects, so a
      suggestion for a `list[str]` parameter is heuristic: labelled `list[str]`, left out of the
      default list and of `--diff`, and shown with `--all`. Widening a parameter keeps every
      caller valid, and only a subclass outside the project that overrides the method with `list`
      gets a type error of its own (T2), so a suggestion on a name with no leading underscore
      along its dotted name is shown, labelled `public`, and ranked like the others. A group
      that holds an override family is labelled `family`.
  12. **Paths select what is shown.** `heki recommend [PATHS]` analyzes the project the paths are
      in and shows what concerns them: a directory gives the advice for that subpackage, a file
      its suggestions. This replaces ADR-020 rule 8, under which a narrower path narrowed the
      analysis and lowered every confidence, which no use of the command wants. `--touched
      FILE...`, as in `heki check`, shows the groups with an edit in one of the files, and
      `FILE:FIRST-LAST` the groups whose function overlaps those lines, for an agent that
      changed part of a file.
  13. **Generated and vendored code.** A file whose leading comment says it is generated
      (`generated`, `DO NOT EDIT`) gets no suggestion, nor does a function a stub (`.pyi`)
      declares, and the notes count them: the next generation drops the edit, and the type
      checker reads the stub, not the edited source. Vendored directories are left to `exclude`,
      since a directory name is no fact about the code.
  14. **Projects without a type checker.** Read-only parameters are suggested with a note, since
      `MUT110`'s half of their guard runs without one; `Final`, `@final`, and `Literal`, which
      only the type checker would guard, are held back.
  15. **Ratchets.** `heki recommend` also proposes changes that cost nothing today and report only
      regressions: an exact rule that is off and has no report here, named in `extend-select`
      (experimental exact rules such as `MUT101` and `MUT111` in a list apart that says they are
      experimental), and an owner contract with `unresolved = "ignore"` whose trial with
      `"report"` reports nothing. A heuristic rule is never proposed this way: a rule with no
      report today can still print a false one tomorrow (ADR-018).
  16. **No dismissal in the first release.** Suggestions are shown when asked for, the editor's
      hint follows only edited functions, and `MutableSequence` or `MutableMapping` states the
      opposite intent in the code, which heki never proposes to narrow. There is no new
      suppression syntax and no configuration. If users ask for a dismissal, it is
      `[tool.heki.recommend] ignore = [<id patterns>]`, in kebab-case under `[tool.heki]`
      (ADR-017).
  17. **No `@override`.** heki knows every override and could print the patch that marks them,
      but the marker protects against a renamed base method, which the type checkers report
      themselves when asked, and no fact heki establishes depends on it. `@final` is different:
      it keeps an "every implementation" premise established against a later subclass, so once
      the decorator table has a `final` row it is suggested where an exact report or an
      established premise goes through the class or method, and nowhere else. *Rejected*:
      `@override` suggestions for now (to revisit as a `--focus` of its own if users ask).
  18. **A new kind passes the gate of a rule.** Each kind ships after counts on the corpus and the
      certification of ADR-018 decision 12 as amended (a judged sample with no false claim, the
      mechanical checks on every group), and gets a step in `lsp_oracle.rs`. The first kinds are
      read-only container parameters, then attributes bound once while their object is
      constructed, shown only when read in at least 2 modules other than their class's. Container
      constants, read-only results, parameter classes, `Literal`, `@final`, existing read-only
      views, and `ReadOnly` keys each wait for their own measurement.
  19. **Certification of the read-only parameter suggestion.** The claim is that the parameter
      itself is never changed, in the function and in every function it is handed to, and that
      it does not leave. A judged sample of 300 exact groups from 257 packages was read by two
      reviewers independently; they agreed on 300 of 300, and none was false under that claim.
      Five of the 300 change an element reached through the parameter (`acc[k].append(...)`, or
      a write into dicts held by a list). The claim allows this, since `Mapping` and `Sequence`
      do not restrict the elements, but a reader may take "only read" to mean deep, so the
      message says that the parameter itself is never changed. The release gates held on the
      whole corpus: whole-corpus time 1.42 times the base (limit 1.5), no failed run, peak heap
      of the recommend run 0.7 % above the base, no failing package, and no difference in the
      reports of the other commands.
- **Status**: decided, not implemented yet. The parts ship under `command.recommend`, which is
  experimental, so what it prints and its JSON object can change in the releases that add them.

---

### ADR-024: Registration contexts: the argument a registration hands a callback, carried to the handler a lookup by name selects
- **Background**: web and command frameworks register a callback per view or command with the
  object that owns them, and the callback calls the handler `getattr(self, <name>)` finds. The
  handler's parameter for the owner is unannotated, so `heki inspect` showed no change through
  it. On an open-source web application, two handlers of this kind write a list of the owner
  through two and three calls; annotating the parameter reproduced the full record, and a
  prototype that derived the owner's class from the registration reproduced it without the
  annotation.
- **Decision**:
  1. A context belongs to one registration and one name: the handler gets the condition "the
     name equals its name", and only that registration's owner class. Two owners registering
     one handler keep two contexts.
  2. Each function's part (rows of a table it returns, the table handed to a class and kept in
     an attribute, the call of a row's slot, the row a helper picks, and the program of a short
     function) is extracted while its IR exists, into `EffectSummary::dispatch`, which Early
     Cutoff compares without positions. The join runs over the index at query time, with one
     budget per registration, a nesting limit, and a cycle stop (ARCHITECTURE section 8,
     docs/development.md "Analysis cost").
  3. The record under a context comes from the existing summaries and chains: the handler's
     module is summarized again with the one parameter type, in a copy of the index. The
     contexts are advisory and shown only by `heki inspect` (`registered`).
- **Alternatives rejected**: typing the parameter from every registration of the handler, which
  is wrong when two owners register it and turns a conditional fact into an unconditional one;
  keeping IR for the join, which ADR-017 forbids; a list of framework names or HTTP methods,
  which breaks on renames and other frameworks.
- **Consequences**: no rule, exact report, or fix reads a context. A registration shape the
  model does not follow gives no context, never a guess; each new shape is a fact of its own
  with true-positive and true-negative tests.

## 5. Open items

- The design, name, license (MIT), interface, and Stage 0-A measurements (Waves 1 and 2) are
  settled. What remains for v1.0 is listed in [ARCHITECTURE.md §10](../ARCHITECTURE.md), and the
  rules for working on the code base in [AGENTS.md](../AGENTS.md).
