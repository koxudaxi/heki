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
