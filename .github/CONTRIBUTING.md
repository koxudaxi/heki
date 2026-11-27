# Contributing

Thank you for helping heki. Read [AGENTS.md](../AGENTS.md) before you change code: it holds the invariants and the
procedures every change follows.

## What we value

- **Problems from real use.** The most valuable contribution is a report of a problem you met while running heki on
  your own code: the minimal Python input, the exact command, the configuration, what you got and what you expected.
  A report that is wrong about the code, above all an exact one, is the most important kind. Use the
  [issue templates](https://github.com/koxudaxi/heki/issues/new/choose).
- **Changes from reading the code.** A pull request is welcome when it fixes a problem you met, or one you found by
  reading the code and reproduced on the current `main`. Say which in the description, in your own words.
- **Your use case as a test.** A pull request may add your use case as an end-to-end test: a minimal Python input that
  shows the case (share only what you may publish), the command, and the output. From then on, every change is tested
  against it. We can't promise to support every use case forever, but we will do our best to keep it working.

## AI tools

You may use AI tools for any part of your contribution, and you don't need to label it. You are responsible for what
you submit, however it was produced: read and run it, and be ready to explain every line. Do not let an agent open
issues or pull requests without your review.

## Pull requests

- **Link each pull request to an issue** (`Fixes #123` in the description). If there is none, open the issue first,
  with the problem and how you met it. Small documentation fixes may skip the issue.
- **Planned work stays with the maintainer.** Issues that the maintainer filed and assigned to themselves are planned
  work. Please don't open pull requests for them unless the maintainer asks for one in the issue; comment there
  instead if the problem affects you, with your use case.

Pull requests without a use case or a reproduction, and pull requests for planned work, may be closed without comment.
This keeps the maintainer's time for the reviews that help users.

## Required reading

In this order, as [AGENTS.md](../AGENTS.md) lists:

1. [ARCHITECTURE.md](../ARCHITECTURE.md)
2. [docs/scope.md](../docs/scope.md)
3. [docs/ANALYSIS_AND_KB_SPEC.md](../docs/ANALYSIS_AND_KB_SPEC.md)
4. [docs/DESIGN_HISTORY_AND_ADR.md](../docs/DESIGN_HISTORY_AND_ADR.md)

When a document and the code disagree, update the document in the same change.

## Checks

Run these before you open a pull request. The development guide is [docs/development.md](../docs/development.md).

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# Every line is covered except three parser-rejected arms (docs/development.md).
cargo llvm-cov --workspace --summary-only --fail-uncovered-lines 3
# The Soundness lane must not depend on the Advisory lane: no `heki_effect` here.
cargo tree -p heki_soundness -e normal,build,dev
cargo run -p heki -- rule --all

# heki_semantic against the CPython symtable, both ways (zero disagreements; CI runs 3.11-3.15)
python3.15 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/
# Layer 1 knowledge base sentinels
python3.15 tests/sentinel/verify_soundness_kb.py
# Adversarial Soundness counterexamples (every one Blocked or Disproved)
python3.15 tests/difftest/counterexamples.py
python3.15 tests/difftest/soundness_e2e_counterexamples.py
# Shapes the IR models (frames, bindings, paths); CPython 3.15 only (template strings, PEP 798)
python3.15 tests/difftest/ir_counterexamples.py
# Generated documentation
cargo build --release -p heki
python3.15 scripts/build_docs.py --heki target/release/heki --check
python3.15 scripts/build_llms_txt.py --check
# Peak memory of `heki check` and the `--watch` engine on a generated project
python3.15 scripts/check_memory.py --heki target/release/heki
# The scripts of the release workflows
uvx --python 3.12 --from "pytest==8.4.2" --with "pyyaml==6.0.3" --with "jsonschema==4.25.1" pytest scripts/tests
```

## New rules

A new rule has to pass the gate in [docs/scope.md](../docs/scope.md#how-a-rule-gets-into-heki): what a single-file
syntax check, a type checker, or an import-graph checker can decide belongs to Ruff, ty, or tach. heki takes only what
needs control flow, the call graph, or alias facts.

A new rule is also measured on a corpus and reviewed by hand before it is enabled by default. The maintainer runs the
corpus: say in the pull request whether the rule can make exact reports, and the maintainer takes it from there.

Rules the earlier measurements rejected are not revived without new evidence; [AGENTS.md](../AGENTS.md) lists them.

## Code and tests

- Every new rule and every filter change comes with true-positive and true-negative tests. Write them as end-to-end
  tests through the real binary, in `crates/heki/tests/`.
- Everything in the repository is written in English: code, comments, documentation, tests, and commit messages.
- Say what the code does through names and structure. A comment explains only why: the Python semantics the code
  models, the reason a rewrite or a report is blocked, an invariant, or a constraint the code cannot show.
- Keep development history out of code and tests: no review, branch, or snapshot identifiers, and no measurements of
  particular projects. They belong in commit messages and documents.
