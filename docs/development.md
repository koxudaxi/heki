# Development

This page covers building and testing heki, its checks against CPython, the memory budget, the
documentation, releases, and the crates. [`AGENTS.md`](https://github.com/koxudaxi/heki/blob/main/AGENTS.md)
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
- The Docs workflow deploys the site to Cloudflare Pages (heki.koxudaxi.dev) once a published
  release is on PyPI (the newest release only), or when it is run by hand on `main`, so the site
  documents the released version. That job runs in the `docs-production` environment, which
  admits only `main` and release tags.
- A pull request from this repository gets a preview deployment of the same Pages project, on the
  branch `pr-<number>`. Both use the repository secrets `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID`.

## Releases

The Release Draft workflow (`.github/workflows/release-draft.yml`) keeps a draft GitHub release
of the next version. Publishing the draft runs `release.yml`, which builds and publishes heki.

### How a draft accumulates

After each push to `main`, the workflow takes the pull requests merged at the commits the push
added (several when a stack merges at once). For each one, Claude reads the merged diff and
decides whether it breaks something the newest published release has: which rules are enabled
by default, a rule's default severity or what a default rule reports, rule codes or names,
configuration keys, command-line options, exit codes, the JSON or SARIF output, language server
behavior, the format of `--fix` rewrites, or the supported Python versions and platforms. New
opt-in rules and new optional keys are not breaking. The workflow then:

- adds the breaking changes to the draft's `## Breaking Changes` section, under one heading per
  kind, and a line `* <title> (#<number>)` to `## Changes`, except for pull requests labeled
  `maintenance` or `dependencies`;
- labels a breaking pull request `breaking-change`, comments the result on every pull request,
  and labels each `breaking-change-analyzed`, so that a re-run skips it.

The draft's tag is the version after the newest published release: a breaking change bumps the
minor version (0.1.x to 0.2.0), anything else the patch, and manifests that already name a
later version win. Text added to the draft by hand is kept. Pull requests that change only
tests, documentation, development scripts, or the CI workflows skip Claude and count as not
breaking. While no release is published, the workflow does nothing: the notes of the first
release come from `CHANGELOG.md`.

### Publishing a release

1. While the manifests declare another version, the draft's first line asks for a bump. Set the
   draft's version in a pull request: `pyproject.toml`, `Cargo.toml` (`workspace.package.version`
   and the internal crates under `workspace.dependencies`), and `Cargo.lock`. Label it
   `maintenance` to keep it out of the list of changes. `release.yml` fails when the tag and the
   manifests differ.
2. Merge it; the workflow removes the first line from the draft.
3. Publish the draft on GitHub.

### Replays and dry runs

```console
$ gh workflow run release-draft.yml --ref main -f pr_number=123
$ gh workflow run release-draft.yml --ref <branch> -f pr_number=123 -f dry_run=true
```

- A replay on `main` analyzes one merged pull request again, for example after a failed run. A
  pull request labeled `breaking-change-analyzed` is skipped: remove the label first.
- A dry run works from any branch, so a change to the workflow can be tried before it is on
  `main`. It prints the analysis, the planned draft, and the comments in the run's log, and
  changes no label, comment, or release. The pull request must be merged into `main`, and a
  release must be published.

The Claude step authenticates with the repository secret `CLAUDE_CODE_OAUTH_TOKEN`, an OAuth
token of a Claude subscription (`claude setup-token` creates one). No other step receives it.

The scripts of the workflow have tests, which CI runs on every pull request:

```console
$ uvx --python 3.12 --from "pytest==8.4.2" --with "pyyaml==6.0.3" --with "jsonschema==4.25.1" pytest scripts/tests
```

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
