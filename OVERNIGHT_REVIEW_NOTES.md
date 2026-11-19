# Overnight review notes (M6–M9)

Decisions taken without the owner, per the overnight instructions: gaps in the Soundness lane were
closed conservatively (`Blocked`), gaps in the Advisory lane toward fewer false positives. Each
entry says what was decided and why, so it can be reviewed and reverted one by one.

## M6 — `heki_soundness`, `OPT` rules, `--fix`, `MUT103`

### Soundness lane (`heki_soundness`)

1. **Allocation-site abstraction and strong updates.** `AllocId` is one per allocating IR
   instruction; a site inside a loop summarizes every object it creates, so a variable that still
   holds an object from an earlier iteration stays in that site's alias set. Every SSA definition
   first removes the variable from all alias / derived sets (loops re-define SSA variables).
   Phis are evaluated per edge as parallel copies.
2. **`FrameState.points_to` added.** The spec's `UniqueState::Shared` carries no allocation ids,
   which would hide a mutation made through a shared alias (`m = lst if c else other`, `other`
   escapes, `m.append(...)` mutates `lst`). `points_to` keeps the sites of every variable whether
   `Unique` or `Shared`; mutations and escapes go through it. `alloc_elems` (element kinds of
   lists / tuples / sets) was added for `ZeroTripExceptionSafe` and the probe check.
3. **Exception edges.** The IR's handler phis only see the last binding of each block. The
   exception edge therefore carries the join of the states at every point of the block, and every
   source name with several SSA versions present is joined over all of them.
   `ExceptionStateSafe` additionally blocks a rewrite when an alias of the target with another
   name is read anywhere reachable from a handler of a block after the write (the stale-binding
   case standard SSA liveness misses).
4. **`return` / `raise` are escapes.** The IR does not route `return` through `finally`; marking
   the returned value as escaping makes the exception edge (which does reach the `finally` block)
   see it as shared (`return x` in `try` + a loop rewriting `x` in `finally`).
5. **A1 hazards force `Blocked`** even when another obligation is `Disproved` (spec 4.1.3:
   "即 `Blocked`"); the other obligations are still evaluated and listed.
6. **Builtin shadowing (A1(b)).** A builtin name is trusted only when no module-level binding,
   `global` store, `globals()` call, module-level A1 hazard, or `from m import *` exists in the
   module, and no module of the project imports `builtins` / touches `__builtins__`. The IR gained
   `ModuleIr.has_star_import` (backward compatible, `#[serde(default)]`) because star imports
   were dropped silently.
7. **Module and class bodies are not analyzed for `OPT`**: their namespaces are observable
   (globals, metaclass `__prepare__`).
8. **No Layer 1 KB growth.** Methods missing from `kb/soundness/builtins.toml` (`dict.get`,
   `list.index`, ...) are unknown calls and make their receiver `Shared`. Extending the KB needs
   sentinel coverage (`tests/sentinel/verify_soundness_kb.py`), so it was left for the owner.
9. **Annotations are never read** (S9b). `heki_soundness` gained a dev-only dependency on
   `heki_semantic` for its tests; `heki_effect` stays absent (`cargo tree -p heki_soundness`).

### `OPT` rules

10. **Counterexample gate shape.** In `tests/difftest/counterexamples.py`, S24 and S9b are not
    inside a loop, so no rule matches them there. The gate test runs on the file as is (zero
    `Safe` candidates; the in-loop S23 / S26 / dcg / S27 / S22 are `Disproved` / `Blocked`) and on
    rule-shaped versions of all seven in the new `tests/difftest/opt_rewrites.py`, which also
    asserts on CPython that every naive rewrite diverges. `counterexamples.py` is unchanged.
11. **What is reported.** `Disproved` candidates are never reported (ARCHITECTURE 3.3 "診断抑止").
    `Safe` and `Blocked` candidates are reported only with *cost evidence*: an exact builtin
    container (an accumulation that starts as an exact `list` / `dict` / `set` / `bytes` /
    `tuple`, or an exact list for membership / queue rules). `x in param` with an unknown type is
    not reported: nothing shows a linear scan (Advisory-side noise suppression).
12. **Profitability notes** (`not_profitable`): small constant displays (at most 8 elements,
    never mutated) for `OPT110` / `OPT130`, and `ConstSmall` / retry loops for the other
    size-dependent rules. Such candidates are not reported.
13. **`HashConsistentProbe` (new obligation, `OPT110` / `OPT111`).** The spec checks the list's
    elements only, but `e in set_` can differ from `e in list_` when the *probe* has a custom
    `__eq__` without a matching `__hash__` (or is unhashable). The probe must be a builtin
    `int` / `str` / `bytes` / `bool` / `float` (or come from an empty literal).
14. **`OPT102` edits** keep evaluation order: `d[k] = v` only for one `k: v` entry whose key and
    value are constants or bound locals (no side effects, cannot raise); otherwise
    `d.update({...rest...})` evaluates the remaining entries in their original order before
    changing `d`; `{**d, **e}` becomes `d.update(e)` only when `e` is an exact dict (mapping check
    preserved), else `d.update({**e})`. `s = s | rhs` needs an exact `set` / `dict` right operand.
15. **`OPT103` has no edit** (spec: diagnostic + suggestion only): the buffer rewrite needs
    statements before and after the loop. Local `str` accumulation is excluded (CPython's
    in-place optimization); `self.s += ...` is reported when the right operand is an exact `str` /
    `bytes` / `tuple`, always `Blocked` (every reader of the attribute would need the joined value).
16. **Size-dependent edits.** `OPT110` / `OPT130` hoist before the `for` statement (not for
    `while` loops or comprehension loops, whose statement start is not in the IR); `OPT111` adds
    `seen_set = set(seen)` and `; seen_set.add(v)` after the paired append; `OPT120` / `OPT121`
    wrap every allocation of the list in `collections.deque(...)` (or a bound `deque`), rewrite the
    site to `popleft()` / `appendleft(...)`, and insert `import collections` before the first
    column-0 non-`__future__` import when needed (no import at all: no edit). `DequeEquivalentOps`
    rejects any use without a deque equivalent (sorting, slicing, middle indexes, `==`, returning
    or passing the list on).
17. **`OPT111` `ExclusiveAccessAtWrite`** is evaluated without requiring `seen` itself to be dead
    (the append keeps it bound), and a live alias is `Blocked`, not `Disproved` (it could change
    the list behind the auxiliary set, which is not a proven counterexample).

### CLI

18. **`--fix` iterates** (like Ruff): up to 10 in-memory passes, each re-proving every candidate on
    the previous pass's text, then one write per file. `--diff` prints the same final result
    without writing; it exits 1 when it would change something, 0 otherwise.
19. **Applicable fixes:** `OPT101` / `OPT102` (`auto_fix_default`) always; size-dependent fixes
    with `--include-size-dependent`, or when `--profile` shows the function on the stack in at
    least 1 % of the samples (`heki_profile::HOT_THRESHOLD_PCT`). Overlapping fixes are skipped,
    not merged; suppressed diagnostics are not fixed.
20. **`--profile`** reads Tachyon `--collapsed` (`file.py:func:line` frames), py-spy collapsed
    (`func (file.py:line)`), and Tachyon `--gecko` JSON (format taken from CPython 3.15's
    `profiling/sampling/gecko_collector.py`; a live dump could not be produced on macOS without
    `sudo`). Matching uses the function name and a file-path suffix; hot functions also get a
    `profile: ... % of samples` note in the `OPT` diagnostic's `evidence`.
21. **`heki snapshot [PATHS...] [--write]`** prints or writes `.heki-summary-snapshot.json`
    (version 1, keys `module:qualname`, parameters by name). Public = name without a leading
    underscore, not nested in a function. `heki snapshot summary --write` (ARCHITECTURE spelling)
    is accepted. `MUT103` reports a function whose summary gained an element in any of the four
    sets; with `MUT103` enabled and no snapshot file, `heki check` warns and reports nothing.

### Dogfooding (read-only `~/work/dcg-1/src`, release build)

- 419 files in 1.33 s; diagnostics: `MUT101` 16 (unchanged from M5), `OPT110` 2 (`Blocked`:
  element types unknown), `OPT120` 1 (proven-safe size-dependent suggestion: a BFS queue using
  `pending.pop(0)`). The default `--fix` would change nothing (ARCHITECTURE 3.3 expects 0 for dcg).

## M7 — `heki server` / `heki lsp` and `--watch`

1. **One project pipeline.** Discovery, configuration, parsing, and inline-suppression
   filtering moved from the CLI into `heki_rules::project` (`Project`, `ProjectEnv`,
   `parse_source`, `retain_unsuppressed`), shared by `heki check`, `heki snapshot`, `--watch`,
   and the LSP engine.
2. **Resident state (ADR-017).** `heki_lsp::Workspace` keeps configuration, knowledge bases,
   file metadata, the `ProjectIndex`, and the project declarations; IR is built for one file at a
   time and dropped. A source-level guard test (`invariant_tests`) fails if `Workspace` or the
   server state gains an IR / semantic-model field.
3. **Edit flow.** Full text sync. `didChange` (250 ms debounce) re-analyzes only the edited
   document and applies `update_file`; files it affects (callers of changed summaries, and
   importers when the module's declarations changed) are queued and re-analyzed on `didSave`.
   Importers = files whose runtime or `TYPE_CHECKING` imports name the module, a submodule, or
   a parent package.
4. **Per-file checks in the editor.** The engine checks one file against the index, so
   `EFF101` / `EFF105` root-cause grouping (10+ boundaries collapsed into one diagnostic) groups
   within the file, while `heki check` groups project-wide. Chains in other files keep the spans
   of their last propagation after a position-only edit (the M4 Early Cutoff ignores spans).
5. **`heki/stats` request** (not part of LSP) returns the engine counters; the integration test
   uses it to prove Early Cutoff (`last_summary_changed == false`, `dependent_reanalyses == 0`
   after a comment-only edit).
6. **Exit.** tower-lsp only leaves its read loop at stdin EOF, even after `exit`. Editors close
   stdin; for clients that do not, the server ends 2 s after `shutdown`. The runtime is shut
   down in the background so the blocking stdin reader cannot keep the process alive.
7. **Code actions.** `quickfix` for `OPT101` / `OPT102` fixes (preferred), `refactor.rewrite.heki`
   for proven-safe size-dependent rewrites, and a `quickfix` appending
   `  # heki: ignore[CODE]` at the end of the diagnostic's line for every diagnostic.
   `Diagnostic.related` hops became `relatedInformation` with exact UTF-16 ranges;
   `RelatedLocation` gained a `span` field (also visible in `--output-format json`).
8. **`--watch`** polls modification times every 200 ms (no watcher dependency), re-analyzes
   changed files through the same engine, re-checks affected callers / importers immediately,
   and reloads everything when `pyproject.toml`, `heki.toml`, `.heki.toml`, `--config`, or the
   summary snapshot changes. It prints the full diagnostic list after every change; `--fix`,
   `--diff`, `--baseline`, and `--update-baseline` are ignored with a warning. The exit code is the
   last report's. `HEKI_WATCH_MAX_RUNS` (tests only) stops it after N re-checks.
9. **Incremental cost.** Measured on the read-only `~/work/dcg-1` workspace (13,166 files
   including test data; release build): initial index 4.5 s in the background; edit latency
   after the debounce ~45 ms for a 953-line file (dcg p90), ~85 ms for 4,086 lines, ~650 ms for
   the 9,482-line `parser/jsonschema.py`. To get there, `ProjectIndex::update_file` no longer
   scans the whole project: call sites are loaded only for the closure's files, the SCC closure
   expands callers first, `sync_driven_only` is recomputed for the edited file's functions and
   their callees, and the engine keeps `ProjectDecls` up to date in place
   (`reanalyze_module_cached`). The remaining cost on very large files is the re-propagation of
   every function of the file: classes with hundreds of methods copy the union of their
   callees' `self` attribute sets on every edge. Interning attribute keys would fix that; it
   changes the M4 summary representation, so it is left for the owner.

## M8 — E2E-first test suite and coverage

### How the suite is built

1. **End to end first.** New tests drive the real binary (`CARGO_BIN_EXE_heki`) and the real
   `heki server --stdio`: `e2e_frontend.rs` (dump commands across py311–py315, `assumptions`,
   `rule` / `explain`, the CPython symtable verifier over `tests/corpus/`), `e2e_advisory_cli.rs`,
   `e2e_advisory_effects.rs`, `e2e_advisory_mut.rs`, `e2e_soundness.rs`, plus new cases in
   `cli_fix.rs`, `cli_watch.rs` and `lsp_server.rs`. Unit tests were added only for limits,
   invariant guards, I/O errors that a process cannot provoke portably, and the existing
   obligation-stripping tests.
2. **Corpus.** `tests/corpus/frontend_tour*.py` exercise every statement and expression form
   the frontend lowers (Python 3.11 syntax, 3.12 type parameters, 3.14 template strings and
   `except A, B:`, PEP 810 lazy imports, `from __future__ import annotations`). They agree
   with CPython `symtable` on 3.11, 3.12, 3.14 and 3.15 and are checked in CI.
3. **Dead code is removed, not tested.** Branches that cannot run were deleted or folded into
   the code path that does run. Each removal states its invariant in a comment where it is not
   obvious. Evidence for the frontend (IR builder, semantic model): `dump-ir` and
   `dump-semantic` output byte-identical before and after on 3,706 real files (CPython 3.15
   stdlib, 3.14 site-packages, datamodel-code-generator `src`, corpus, fixtures), and an
   instrumented build showed that none of the removed branches was ever reached there (one
   that was, the empty back edge in `compute_loop_carried`, was kept and tested). For the
   advisory crates and the LSP, every removed branch follows from an invariant of the caller
   (for example: SSA copies always define a fresh variable, so the copy walk in
   `drives_manually` cannot revisit one; `func_files` only names indexed files; `rel_posix`
   never yields `.` components). Unused public helpers with no caller were deleted
   (`InlineIgnores::is_empty`, `Workspace::index` / `paths`, `RuleSelection::enabled_codes`,
   `HekiConfig::digest` with the `blake3` dependency of `heki_env`; the Layer 3 cache it was
   meant for does not exist yet). The soundness lane got the same treatment, and its E2E tests
   alone cover every analysis function through the binary. The `BuiltinKind` ⇄ name table, which
   `heki_soundness` and `heki_effect` each had, now lives once in `heki_ir`
   (`BuiltinKind::ALL` / `name` / `from_name`).

### Defects found by the suite and fixed here

4. **Crash on non-ASCII string constants.** The `EFF103` DDL-prefix test sliced the first
   argument of every call in a loop by bytes and panicked inside a multi-byte character (exit
   101 on the CPython stdlib and on pip's vendored `rich`). It now compares bytes; regression
   test `eff103_reads_query_constants_with_non_ascii_text`.
5. **Error causes were printed twice** (`... missing.toml: No such file or directory (os error
   2): No such file or directory (os error 2)`). Thirteen error variants both embedded their
   cause in the message and returned it from `source()`. Following the Rust API guidelines
   (one or the other, never both), the messages keep the cause and the field is no longer a
   `source()`, so the CLI (`{:#}`), the LSP and warnings print it once.
6. **A failed baseline write said "failed to read baseline".** `BaselineError` has separate
   `Read` and `Write` variants now.
7. **`target-version` was not inferred from `requires-python`** (ARCHITECTURE §9.2). The
   loaders now read `project.requires-python` from the `pyproject.toml` that holds the
   configuration, or the one next to a `heki.toml` / `--config` file. The bound is the highest
   lower bound of the `>=`, `>`, `~=`, `==`, `===` clauses (`>3.11` admits 3.11.1, so it gives
   3.11), clamped to the supported 3.11–3.15; precedence is `--target-version`, then
   `target-version`, then `requires-python`, then py312. A malformed or non-string value, or a
   broken neighbouring `pyproject.toml`, only loses the inference (no new configuration error).

8. **Non-UTF-8 file names emptied the JSON report.** On Linux a project with such a file name
    printed `[]` for `--output-format json` while the exit code still reported diagnostics
    (serializing the path failed and the fallback printed an empty list). Paths are now written
    lossily (`Diagnostic.path`, `RelatedLocation.path`); UTF-8 paths are unchanged.

9. **`MUT103` false positive on conditional redefinitions** (Advisory lane, decided toward
    fewer reports). `if sys.version_info ...: def push(items, extra) else: def push(items)` has
    one merged summary, but the snapshot named its parameters through the last definition and
    the check through each definition, so unchanged code reported "mutates parameter `extra`".
    Both sides now go through the last definition of each key, and a real change is reported
    once, there. The TP / TN regression is an end-to-end case in `e2e_advisory_effects.rs`
    (MUT103 needs a saved snapshot, which the single-run `tests/semantic/fixtures.py` format
    cannot express).

### Soundness lane: rewrites that were `Safe` but changed CPython behaviour (fixed)

Found while covering `heki_soundness` end to end. Each case is in
`tests/difftest/soundness_e2e_counterexamples.py` with the rewrite the M6 code produced; the
script asserts on CPython that all 17 old rewrites diverge, and the end-to-end test asserts that
heki now leaves the file unchanged. Every fix only adds `Blocked` / `Disproved` or drops an edit;
the M6 gates (`cli_fix.rs`: 15 default fixes, 20 with `--include-size-dependent`) are unchanged.

10. `OPT102` in the default `--fix`: `d = {**d, "prev": d}` became `d["prev"] = d` (a cycle) and
    `d = {**d, k: iter(d)}` became `d.update(...)` (the stored iterator then raises
    `RuntimeError`). `Unique(d)` is now checked on the state after the display, whose entries
    may store `d` itself or a view / iterator of it.
11. `OPT110` / `OPT111` / `OPT130` hoisting: the `for` statement's iterable runs after the
    hoisted `set(...)`; a walrus rebinding there, or a change anywhere in a multi-block iterable
    (`for p in (allowed.pop() and range(n))`), was missed (the M6 check looked at the preheader
    block only). Both are now checked over the iterable's span in every block it spans.
12. `OPT130`: calls nested in the rebuilt expression were treated as pure
    (`set(range(int(s)))` raises, `tuple(list(map(repr, objs)))` runs user code even when the
    loop runs zero times); `list(5)` was judged unable to raise; `set(d)` / `frozenset(d)` /
    `dict(d)` of a dict compares keys with equal hashes (user `__eq__`). All blocked now.
13. `OPT110` / `OPT111`: the hoisted `set(...)` resolved to a local `set` (a parameter, or a name
    of an enclosing function). Blocked.
14. `OPT120` / `OPT121`:
    - The edit wrote `collections.deque` where the function or an enclosing one binds
      `collections`. No edit now. The enclosing functions are found by span containment
      (the first fix compared qualnames, which miss a function around a class: see item 21).
    - A second `q.pop(0)` was taken as deque-equivalent, but `deque.pop()` takes no index.
    - Iterating the list while it changes raises `RuntimeError` once it is a deque.
    - `q = q[1:]` on an empty list is `[]`, but `popleft()` raises. It is now accepted only
      directly under `while q:` testing that `q`.
    - `import collections` was inserted after module code that may already call the function.
      It is now inserted only after a docstring and `__future__` imports.
    - The `deque` spelling was trusted in modules whose globals change behind the IR (star
      import, module-level `exec`, `globals()`).
15. Two smaller conservative corrections came out of the dead-code review:
    - Joining name groups took one side's exact type when the other side was untracked. It is
      now untracked.
    - `definitely_bound` treats a variable without a definition as unbound.

Open question: `OPT102` and `s = s | {...}` call user `__eq__` a different number of times on
hash collisions when updating in place instead of rebuilding a copy. This is kept as an
implementation detail (CPython does not specify probe counts). Blocking it would block the
`OPT102` positives themselves.

### Defects found and left for the owner (behaviour changes or design questions)

16. **A `.pyi` stub next to its module replaces the module's effects and imports.** Both files
    get the same module name and the last one indexed wins; with `pkg/util.py` blocking and a
    stub `pkg/util.pyi`, the `EFF101` on callers disappears, and later edits can leave callers
    whose callee has no defining file. False negatives only. Declarations come from the last
    file with the module name too, so `pkg/api.pyi` next to `pkg/api.py` hides `api.py`'s own
    imports (its `EFF101` disappears). Proposed: prefer the `.py` file for the index when both
    exist (CLI and LSP).
17. **Long call cycles stop propagating.** The SCC fixpoint keeps the spec's safety limit (16
    iterations per member). A plain cycle of 20 methods that each write a `self` attribute
    exceeds it because the worklist visits callers before callees; some members end with 18 of
    the 20 attributes. The SCC is marked `unknown_calls`, which no rule reads, so the result is
    silent false negatives (MUT101, MUT201, MUT103, EFF101) in large recursive groups such as
    recursive-descent parsers. Proposed: seed each SCC's worklist callee-first (the Tarjan
    finishing order), which converges a simple cycle in two rounds, and keep the limit as a
    guard.
18. **`EFF103` misses N+1 queries on connections from lowercase factories.** The result of
    `sqlite3.connect(db)` (also `psycopg.connect`, `boto3.client`) is typed as the factory
    itself, so `conn.execute(...)` resolves to `sqlite3.connect.execute`, which the KB does not
    know. A local annotation (`conn: sqlite3.Connection = ...`) does not help: annotations are
    used only when inference finds nothing. Proposed: KB return types for factories.
19. **`MUT103` does not see a function's own new effects.** DETAILED_SPEC §4.7 stores
    `transitive_exec_effects` (effects inherited from callees), while ARCHITECTURE describes
    MUT103 as catching any new blocking effect. Kept as specified (the Advisory lane decides
    toward fewer reports); storing `direct ∪ transitive` would report a public function that
    starts calling `time.sleep` itself.
20. **Frontend vs CPython `symtable`, outside the verified fixtures:** on 3.14+ the module /
    class `__annotate__` scope is opened after the whole body (CPython: at the first
    annotation); TypeVar bounds, constraints and defaults get no lazy scopes of their own;
    the synthetic `.type_params`, `.generic_base` and `__type_params__` symbols of generic
    classes are missing; a `global` / `nonlocal` name never assigned in that scope is reported
    as a binding. These forms are kept out of the verified corpus and documented in the tests.
21. **Class qualnames ignore function nesting**: a class defined in `f` is `Local`, not
    `f.<locals>.Local` (and `A.m` → `A.B` instead of `A.m.<locals>.B`); IR qualnames inherit
    this and can collide with a real top-level class of the same name. The soundness lane no
    longer depends on them (item 14).
22. **The SSA form is not minimal**: phis whose users become trivial are not re-checked, so
    redundant loop-header phis remain and read-only values can be reported as self-accumulating
    loop-carried variables of outer loops (for example the `output` parameter of CPython's
    `base64.encode`). DATA101 additionally requires a quadratic call, so no false positive was
    seen, but the loop facts are imprecise.
23. **Minor**: `clients.hook()` (a module-level instance called through its module) is not
    resolved; in a project whose root is itself a package, `from . import util` in a top-level
    module resolves to nothing while `from .util import helper` works; a new unsaved file opened
    before indexing finishes gets only `OPT` diagnostics until its first edit; the "exclude set
    too large" error prints the whole list; folding `-1` emits a dead `Const 1` first.
24. **Seen by the soundness work, left as is (conservative):** `list.count` / `list.index` are not
    in the Layer 1 KB, so using them makes a list shared (M6 decision 8: the KB is not grown
    here). In the IR, a `try` block that emits no instruction (`try: return x`) records no
    exception edge, so `finally` sees `x` as possibly unbound.

### Coverage

25. **Result** (`cargo llvm-cov --workspace`): lines 15,134 with 3 missed (99.98%), functions
    1,328 with none missed (100%), regions 99.18%. The `--show-missing-lines` list and the summary
    table name the same 3 lines: `crates/heki_ir/src/builder.rs` 1209, 1288 and 1343, the
    `_ => {}` arms of the matches over ruff's `Expr` for augmented-assignment, assignment and
    `del` targets. ruff's parser rejects every other target (`InvalidAugmentedAssignmentTarget`,
    `InvalidAssignmentTarget`, `InvalidDeleteTarget`) and heki refuses files with syntax errors,
    so they cannot run. Covering them would need hand-built invalid trees, and an instrumented
    build over about 5,000 real files never reached them.
26. **Summary table vs missing-lines list.** The summary (what `--summary-only` and
    `--fail-under-lines` use) counts the unit-test build and the normal build of a function as
    two instantiations and keeps the better one. It also counts a closure that never runs as a
    missed line even when its line ran. The suite is arranged so that one build covers each
    function completely: end to end for the analysis, and unit tests only for small guard
    functions. That rule also exposed the untested error closures fixed above.
