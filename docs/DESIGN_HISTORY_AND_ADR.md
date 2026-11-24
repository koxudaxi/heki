# Design History, Revision Log, and Architecture Decision Records (ADRs)

> **What this document is**
> The supplement to [ARCHITECTURE.md](../ARCHITECTURE.md): how the design arrived at its current
> form, the evidence behind it (large-scale measurements: 19.7 million lines plus the Stage 0-A
> Wave 1/2 corpora of 19 million lines, counterexample experiments, a study of
> datamodel-code-generator's 196,000 lines and its 20,000 lines of in-house checkers, and checks
> of CPython 3.15 PEP 810 and `ruff_python_parser` 0.17.5), the reasons for every revision from
> v1.0 to v4.1, and every major decision (ADR-001 to ADR-022). The project's working name was
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
  functions outside the checked files that Layer 2 does not list have no known effects.

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
     library class, leaves it open. A type fact keeps its provenance (constructed here, or an
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
  13. **The default set**: a rule runs by default when it is exact and its share of real harm
      is worth the line it prints. For 0.2.0 that is `EFF101`, `EFF103`, `OPT113`, and the
      proven tier of the `OPT` rules once each is certified; `MUT107`, `OPT112`, and `EFF102`
      stay heuristic. *Rejected*: every exact rule by default (a rule whose reports are right but rarely
      harmful, such as `MUT107` with 4% real harm in round 3, would print lines nobody acts
      on); releasing 0.2.0 with the earlier default set.
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
     A run with the whole block then applies it as `heki check` would.
  3. A catalog entry enters the block only when heki can verify every key it sets. heki cannot
     verify a library's effects, so library entries never enter it; such calls are listed as
     questions, with what a trial entry would report. `src` enters only after its trial.
  4. Annotations and code changes are never in the block.
  5. Likely truth quotes only published measurements, with their sample, interval, and date,
     or the open premises of these reports; what is not measured is said so. No score is
     synthesized.
  6. Confidence levels are measurable conditions.
  7. A hypothetical is printed as a conditional and kept out of the block and out of every
     count of reports.
  8. Paths narrower than the project root give a note, and lower the confidence of contracts.
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
     its timing class (`body`, `lazy`, `function`, `type-checking`), and whether an
     `ImportError` handler takes its failure. The choice between a submodule and an attribute
     lives in `ProjectDecls` and serves name resolution too. No rule resolves an import itself.
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

---

## 5. Open items

- The design, name, license (MIT), interface, and Stage 0-A measurements (Waves 1 and 2) are
  settled. What remains for v1.0 is listed in [ARCHITECTURE.md §10](../ARCHITECTURE.md), and the
  rules for working on the code base in [AGENTS.md](../AGENTS.md).
