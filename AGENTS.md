# AGENTS.md — Rules for Developing and Handing Over `heki`

This repository is **heki**, the third layer of Python static analysis: an **Effect & Alias
semantic checker** (a single Rust binary with a CLI and an incremental LSP server). Every agent
that implements or changes anything here follows the invariants and procedures below.

## 1. Required reading (in this order)
1. `ARCHITECTURE.md`: the overall design, the rule families, and the CLI, configuration, and LSP.
2. `docs/scope.md`: what heki checks and does not check, how it behaves when it cannot tell, and
   the gate a new rule has to pass.
3. `docs/ANALYSIS_AND_KB_SPEC.md`: the IR, `EffectSummary`, the Soundness lane's abstract domain
   and proof obligations, the knowledge bases, and the Stage 0-A measurements.
4. `docs/DESIGN_HISTORY_AND_ADR.md`: ADR-001 to ADR-017, what was decided and why the
   alternatives were rejected.

When a document and the implementation disagree, update the document in the same change.

## 2. Architecture invariants (never break them)
1. **Two physically separated lanes (ADR-004)**:
   - `crates/heki_soundness` never depends on `crates/heki_effect`.
   - `heki_soundness` never trusts PEP 484 annotations; it uses only `EffectiveSoundnessKb`
     (Layer 1) and `RuntimeProfile`.
2. **Parser isolation (ADR-009 / ADR-016)**:
   - Only `crates/heki_ast` depends on `ruff_python_parser` / `ruff_python_ast` /
     `ruff_text_size` (pinned to `tag = "0.17.5"`, which parses Python 3.15 PEP 810
     `lazy import`).
3. **A pure Layer 1 and no dives into known packages (ADR-005)**:
   - `kb/soundness/` never holds advisory effects.
   - The on-demand analysis of unregistered dependencies (Layer 3, planned) never enters the
     standard library or packages registered in Layer 2 (`opaque_packages`).
4. **Safety × Profitability orthogonality (ADR-007 / ADR-011)**:
   - The default `heki check --fix` applies only fixes that are `safe` and `unconditional`
     (`OPT101`, and `OPT102`: `{**d, ...}` with the accumulator leading, and `s = s | t` /
     `d = d | other` on exact operands).
   - Custom decorators, custom contract APIs, Lean 4, native extensions, a general import-cycle
     checker (tach's domain), and framework-specific heuristic rules are non-goals: do not add
     them.
5. **Incremental LSP (ADR-017)**:
   - `heki_effect::ProjectIndex` never keeps IR: it keeps the light `FileSummary`, the reverse
     call graph, and Early Cutoff. Facts a rule needs across functions go into `EffectSummary` /
     `FileSummary` and take part in the Early Cutoff comparison.
6. **One interface with Ruff and `t-linter` (ADR-017)**:
   - Configuration lives under `[tool.heki]` in kebab-case; suppression comments are
     `# heki: ignore[...]` (primary) and `# noqa:` (compatible); exit codes are `0 / 1 / 2`.
7. **English only**: everything in the repository (code, comments, documentation, tests, and
   commit messages) is written in English.
8. **Bounded walks (`heki_ir::walk`)**:
   - Walks over IR ids, the call graph, or the class hierarchy go through `heki_ir::walk`: its
     engines keep each answer per value, join facts with a pass limit, end on cycles, and spend
     one budget per owner.
   - A question an analysis cannot decide answers `Answer::Open`, and each use states what that
     means to it: a rewrite takes it as "no rewrite", an exact report as "no finding".
   - `crates/heki/tests/walks.allow`, checked by `crates/heki/tests/architecture_walks.rs`, only
     shrinks.

## 3. Required verification commands
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
# Analysis cost of generated shapes at the largest size heki models (1 s / 1 GiB per shape)
python3.15 scripts/check_walk_shapes.py --heki target/release/heki
# The scripts of the release workflows
uvx --python 3.12 --from "pytest==8.4.2" --with "pyyaml==6.0.3" --with "jsonschema==4.25.1" pytest scripts/tests
```

## 4. Adding or changing rules
- A new rule passes the gate in `docs/scope.md`: what a single-file syntax check, a type checker,
  or an import-graph checker can decide belongs to Ruff, ty, or tach; heki takes only what needs
  control flow, the call graph, or alias facts. It is also measured on a corpus and reviewed by
  hand before it is enabled by default (ADR-010).
- Every new rule and every filter change comes with true-positive and true-negative regression
  tests: end-to-end tests through the real binary in `crates/heki/tests/`, plus
  `tests/semantic/fixtures.py` or `tests/difftest/` where they apply.
- A rule's documentation lives in `crates/heki_rules/rule-docs/<CODE>.md`; regenerate
  `docs/rules/` and `llms.txt` with `scripts/build_docs.py` and `scripts/build_llms_txt.py`.
- A rule reads other files only through the index or the declarations (never another checked
  unit's IR), so that the language server's single-file check equals `heki check`. Every new
  rule and every new cross-file fact gets a step in `crates/heki/tests/lsp_oracle.rs`, which
  compares the server with `heki check` after each step of an editing session.
- A new walk, fixpoint, or per-function state follows "Analysis cost" in
  `docs/development.md`: it goes through the engines of `heki_ir::walk` (answers kept per value,
  fixpoints that join and have a pass limit, a budget per owner), and comes with a shape in
  `crates/heki/tests/e2e_analysis_bounds.rs` for each bound. Such a change is also run over a large corpus under a memory cap and a timeout
  before it is merged.
- Rules the Stage 0-A measurements rejected (`MUT102a/b`, `DATA102`, `EFF104`, `MUT104` for
  `ALL_CAPS` constants, `MUT105`, `EFF108`) are not revived without new evidence.

## 5. Names, structure, and comments
- Say what the code does through names and structure: name functions, types, and variables
  after what they mean, and split a long function into named steps instead of commenting its
  paragraphs.
- A comment explains only why: the Python semantics the code models (evaluation order, binding
  rules), the reason a rewrite or a report is blocked, an invariant, or a constraint the code
  cannot show.
- A `///` comment on an item adds what its name and signature do not say. It does not restate
  the name, and a call site does not repeat its callee's documentation.
- No development history in code or tests: no review, lane, branch, or snapshot identifiers, and
  no measurements of particular projects. They belong in commit messages and documents.
