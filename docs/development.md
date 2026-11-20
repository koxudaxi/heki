# Development

This page covers building and testing heki, its checks against CPython, the memory budget, the
documentation, and the crates. [`AGENTS.md`](https://github.com/koxudaxi/heki/blob/main/AGENTS.md)
lists the invariants every change must keep and the checks to run.

## Build and test

```console
$ cargo build
$ cargo test --workspace
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo fmt --all -- --check
```

- The tests are end to end first: they run the real `heki` binary and the real LSP server
  (`heki server --stdio`) on small projects.
- The `--fix` tests run the original and the rewritten programs on CPython and require identical
  output. They use `python3.15`, or the interpreter in `HEKI_TEST_PYTHON`.

### Coverage

```console
$ cargo llvm-cov --workspace --show-missing-lines --fail-uncovered-lines 3
```

- Every line is covered except three that cannot run: the `_ => {}` arms for assignment,
  augmented-assignment, and `del` targets in `crates/heki_ir/src/builder.rs`, which the parser
  rejects. CI fails on any other uncovered line.
- The summary counts the unit-test build and the normal build of a function separately and keeps
  the better one, and it counts a closure that never runs as missed. Cover each function
  completely from one side: end to end for analysis code, unit tests only for small guard
  functions.

## Checks against CPython

```console
$ cargo build
$ python3 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/ tests/corpus/
$ python3 tests/sentinel/verify_soundness_kb.py
$ python3 tests/difftest/counterexamples.py
$ python3 tests/difftest/soundness_e2e_counterexamples.py
$ python3.15 tests/difftest/ir_counterexamples.py
```

CI runs them on CPython 3.11 to 3.15 (`ir_counterexamples.py` on 3.15 only).

| Script | Checks |
| ------ | ------ |
| `verify_symtable.py` | heki's scope analysis against CPython's `symtable`: the scope tree (kind, name, definition line, parent), bindings, parameters, `global` / `nonlocal` declarations, and every read. Every target must exist and every file must parse on the running CPython, except fixtures marked for a newer one in their name (`312_315`, `313_315`, `_314`, `pep810`); a run that compares no file fails. |
| `verify_soundness_kb.py` | every claim of the Layer 1 knowledge base (`kb/soundness/builtins.toml`), on the running interpreter |
| `counterexamples.py` | that the naive rewrites heki refuses really change behavior |
| `soundness_e2e_counterexamples.py` | that the rewrites earlier versions of heki made of these shapes change behavior; the end-to-end tests check that heki now blocks or disproves each |
| `ir_counterexamples.py` | that the rewrites heki made before the IR modeled a shape (a binding, an alias, a path) change behavior; needs CPython 3.15 |

## Memory

```console
$ cargo build --release
$ python3 scripts/check_memory.py --heki target/release/heki
```

- CI fails when the peak memory of `heki check` (with and without an ownership contract) or of
  the `--watch` engine on a generated project exceeds its budget in the script.
- The peak is the lowest of three runs: the resident set size on Linux, the physical footprint
  on macOS, each platform with its own budgets.
- The generated project is shaped after what dominates heki's memory on real code bases: typed
  field writes reached through long chains of private helpers (with an ownership contract, every
  caller keeps the write sites they hand over, with their chains), effect chains below
  `async def` entry points, and many modules and classes (every file's check reads the project's
  declarations).

When a budget is exceeded, find what grew before raising it:

- `/usr/bin/time -l` (macOS) or `-v` (Linux) gives the peak of a run;
- a counting global allocator in a scratch build gives the live bytes per phase.

## Analysis cost

heki's time and peak memory grow about linearly with the size of the source. The shapes that
break this are rare in small samples and common in large generated files: a module of tens of
thousands of lines, a function of several hundred. A new walk, fixpoint, or per-function state
follows these rules:

- A walk through the inputs of a phi, or through both operands of an expression, keeps its
  answer per value and question. A depth limit does not bound it: with k inputs per step, depth
  8 is k^8 paths. A loop header's phi is its own input once per `continue`, and chains of
  `isinstance` tests and `match` statements multiply the inputs.
- A fixpoint joins its facts and never replaces them, so each pass can only grow them, and it
  also has a pass limit. Past the limit the facts are unknown, and no rule reports from them.
- Facts are kept per SSA value, not per variable per block. State that grows with a function has
  a budget proportional to the function's source bytes, as the IR and proof-state budgets in
  [scope](scope.md) do. Past the budget the function gets no finding, never a guess.
- A `finally` body is built once per way out of the `try`. Anything made per copy, such as a
  class body, is made once.
- Dead code is modeled too. A loop after a `return` has only its back edge, and copies around it
  (`y = x; x = y`) can close a cycle. A walk along copies ends on such a cycle.
- A rule that follows exception edges, one from each call that can raise, runs a dataflow over
  blocks with one fact per tracked value. It never enumerates paths.

Each bound comes with a shape in `crates/heki/tests/e2e_analysis_bounds.rs` that is slow without
it, checked against a time limit: comparisons sharing an operand thirty deep, many `match`
statements of slicing or pass-through cases, `t1 = t0[t0]` chains in a loop, a loop with many
`continue`s and `isinstance` tests, and a long chain of assignments a loop carries.

A change to the IR builder, a walk, a fixpoint, or a rule's state is also run over a large
corpus of real projects before it is merged, every run under a memory cap and a timeout. No
project may take tens of times more per byte than the rest.

## Documentation

```console
$ cargo build --release
$ python scripts/build_docs.py --heki target/release/heki   # rules and CLI reference pages
$ python scripts/build_llms_txt.py                          # llms.txt / llms-full.txt
$ uvx --from "zensical==0.1.2" zensical serve
```

- Rule documentation lives in `crates/heki_rules/rule-docs/<CODE>.md`: `heki rule <CODE>` prints
  it, and the generator copies it into `docs/rules/`. CI fails when the generated pages are
  stale.
- Experimental features are listed, each with why it is experimental, in
  `crates/heki_rules/src/experimental.rs`. `heki rule <CODE>` and the generated pages (the rule
  pages and [Experimental Features](experimental.md)) read the list, and the generator fails
  when a hand-written page marks a rule differently. An experimental feature can change or go
  away in any release, and the release notes list each change; once it is stable, its entry is
  removed.
- The Docs workflow deploys the site to Cloudflare Pages (heki.koxudaxi.dev) once a published
  release is on PyPI (the newest release only), or when it is run by hand on `main`, so the site
  documents the released version. That job runs in the `docs-production` environment, which
  admits only `main` and release tags.
- A pull request from this repository gets a preview deployment of the same Pages project, on the
  branch `pr-<number>`. Both use the repository secrets `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID`.

## Architecture

The workspace is split by responsibility (`crates/`):

| Crate | Role |
| ----- | ---- |
| `heki_ast` | the only crate that depends on the parser (`ruff_python_parser`, pinned) |
| `heki_semantic` | scopes and bindings, checked against CPython's `symtable` |
| `heki_ir` | SSA control-flow-graph IR with explicit exception edges and loop information |
| `heki_kb` | the knowledge bases: Layer 1 (builtins semantics for the soundness lane) and Layer 2 (ecosystem effects) |
| `heki_env` | configuration loading and the runtime profile of the target Python version |
| `heki_effect` | call graph, per-function effect summaries, incremental project index |
| `heki_soundness` | the alias analysis and proof obligations behind `OPT` rewrites (never depends on `heki_effect`, never reads annotations) |
| `heki_rules` | the rules, rule selection, project discovery |
| `heki_diagnostics` | output formats, inline suppressions, Ratchet baselines, fix application |
| `heki_profile` | offline profile dumps |
| `heki_lsp` | the incremental LSP server and the `--watch` engine |
| `heki` | the command-line interface |

The internal design docs are in the repository:

| Document | Content |
| -------- | ------- |
| [`ARCHITECTURE.md`](https://github.com/koxudaxi/heki/blob/main/ARCHITECTURE.md) | the design |
| [`docs/ANALYSIS_AND_KB_SPEC.md`](https://github.com/koxudaxi/heki/blob/main/docs/ANALYSIS_AND_KB_SPEC.md) | the analyses and knowledge bases |
| [`docs/DESIGN_HISTORY_AND_ADR.md`](https://github.com/koxudaxi/heki/blob/main/docs/DESIGN_HISTORY_AND_ADR.md) | the decisions |
