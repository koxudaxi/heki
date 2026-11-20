# Development

## Build and test

```console
$ cargo build
$ cargo test --workspace
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo fmt --all -- --check
```

The tests are end to end first: they run the real `heki` binary and the real LSP server
(`heki server --stdio`) on small projects. The `--fix` tests run the original and the rewritten
programs on CPython and require identical output; they use `python3.15`, or the interpreter in
`HEKI_TEST_PYTHON`.

Every line is covered except three that cannot run (the `_ => {}` arms for assignment,
augmented-assignment and `del` targets in `crates/heki_ir/src/builder.rs`, which the parser
rejects); CI fails on any other uncovered line:

```console
$ cargo llvm-cov --workspace --show-missing-lines --fail-uncovered-lines 3
```

The summary table counts the unit-test build and the normal build of a function separately and
keeps the better one, and it counts a closure that never runs as missed. Cover each function
completely from one side: end to end for analysis code, unit tests only for small guard functions.

## Checks against CPython

```console
$ cargo build
$ python3 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/ tests/corpus/
$ python3 tests/sentinel/verify_soundness_kb.py
$ python3 tests/difftest/counterexamples.py
```

- `verify_symtable.py` compares heki's scope analysis with CPython's `symtable` (3.11 – 3.15).
- `verify_soundness_kb.py` checks every claim of the Layer 1 knowledge base
  (`kb/soundness/builtins.toml`) on the running interpreter.
- `counterexamples.py` shows that the naive rewrites heki refuses really change behavior.

## Documentation

```console
$ cargo build --release
$ python scripts/build_docs.py --heki target/release/heki   # rules and CLI reference pages
$ python scripts/build_llms_txt.py                          # llms.txt / llms-full.txt
$ uvx --from "zensical==0.1.2" zensical serve
```

Rule documentation lives in `crates/heki_rules/rule-docs/<CODE>.md`; `heki rule <CODE>` prints it
and the generator copies it into `docs/rules/`. CI fails when the generated pages are stale.
The Docs workflow deploys the site to Cloudflare Pages (heki.koxudaxi.dev) once a published
release is on PyPI (the newest release only), or when it is run by hand on `main`, so the site
documents the released version;
only the `docs-production` environment holds that deployment's token. A pull request from this
repository gets a preview deployment in a separate Cloudflare account (project `heki-preview`).

## Architecture

The workspace is split by responsibility (`crates/`):

| Crate | Role |
| ----- | ---- |
| `heki_ast` | the only crate that depends on the parser (`ruff_python_parser`, pinned) |
| `heki_semantic` | scopes and bindings, checked against CPython's `symtable` |
| `heki_ir` | SSA control-flow-graph IR with explicit exception edges and loop information |
| `heki_kb` | the knowledge bases: Layer 1 (builtins semantics for the soundness lane) and Layer 2 (ecosystem effects) |
| `heki_effect` | call graph, per-function effect summaries, incremental project index |
| `heki_soundness` | the alias analysis and proof obligations behind `OPT` rewrites (never depends on `heki_effect`, never reads annotations) |
| `heki_rules` | the rules, rule selection, project discovery |
| `heki_diagnostics` | output formats, inline suppressions, Ratchet baselines, fix application |
| `heki_profile` | offline profile dumps |
| `heki_lsp` | the incremental LSP server and the `--watch` engine |
| `heki` | the command-line interface |

The internal design docs are in the repository:
[`ARCHITECTURE.md`](https://github.com/koxudaxi/heki/blob/main/ARCHITECTURE.md) (the design),
[`docs/ANALYSIS_AND_KB_SPEC.md`](https://github.com/koxudaxi/heki/blob/main/docs/ANALYSIS_AND_KB_SPEC.md)
(the analyses and knowledge bases), and
[`docs/DESIGN_HISTORY_AND_ADR.md`](https://github.com/koxudaxi/heki/blob/main/docs/DESIGN_HISTORY_AND_ADR.md)
(the decisions). [`AGENTS.md`](https://github.com/koxudaxi/heki/blob/main/AGENTS.md) lists the
invariants every change must keep and the checks to run.
