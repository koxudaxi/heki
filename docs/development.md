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

### Run-time oracle

```console
$ cargo build --release -p heki
$ python3.15 tests/oracle/test_checkers.py
$ python3.15 tests/oracle/run_oracle.py --heki target/release/heki --seeds 2000 --time-limit 600 --out /tmp/oracle --minimize
```

The oracle checks heki's exact reports against what the code does when it runs, step 4 of [How
a rule becomes exact](precision.md#how-a-rule-becomes-exact):

1. `generator.py` composes small multi-file projects, one seed each, from a grammar of the shapes
   the exact claims' premises talk about: helpers several calls deep, dispatch over subclasses,
   unions, protocols, and mixins, loads that may raise (a property whose getter raises, a
   subscript, a call) inside and outside `try`, early returns, loops with `break` and
   `continue`, `with` blocks whose manager suppresses or not, functions handed to `map` and
   `sorted(key=)`, `functools` caches, module constants against parameters, and project
   subclasses of the builtin containers. Each project has a `heki.toml` and the entry points
   with their inputs, which make the loads raise one at a time.
2. `driver.py` runs `heki check --output-format json` on the project and keeps the reports whose
   certainty is `exact`. `plan.py` turns each into a claim and the places to watch.
3. `child.py` runs every entry point on its inputs with recording fakes of the catalog calls
   (`fakes.py`: `time.sleep`, `requests`, `sqlite3`, `socket`, `subprocess`, `urllib`, queues,
   `numpy`, `pandas`), so the program runs offline. `monitor.py` records with `sys.monitoring`
   which call was running, which iteration of a loop, and which line, when each fake was called,
   and what each line changed.
4. `checkers.py` refutes a claim from the runs: an `EFF101` call that returned without a waiting
   call during it, an `EFF103` iteration that completed without a statement, a `DATA101`
   iteration whose new value was not built from the previous one, a `MUT110` or `MUT114` line
   that ran without changing the object the report names, an `OPT` accumulator of another type,
   or an `OPT113` inner item with a different key that ran more than the comparison. For the
   `OPT` rewrites, the driver applies `heki check --fix --include-size-dependent` to a copy and
   compares results, exceptions, arguments, output, and library calls on the same inputs.
5. `minimize.py` shrinks a refuting project, running heki again on every candidate, until no
   statement can go.

A refutation is a heki bug unless the program breaks one of the assumptions R1 to R7; the oracle
reports such a case as a violation instead (for example a library call whose deciding argument is
computed, R6), and the generator keeps its programs inside them. The output directory holds
`summary.json` and one folder per refutation with the project, `finding.json`, and with
`--minimize` the shrunk repro in `min/`. Runs take a seed range (`--seeds`, `--start`), stop
starting new seeds after `--time-limit` seconds, and use `--jobs` processes.
`--fail-on-refutation` makes a refutation fail the run. The programs need CPython 3.12 or newer;
CI runs the checker tests and a smoke run of 200 seeds on CPython 3.12 to 3.15.

## Memory

```console
$ cargo build --release -p heki --features count-alloc
$ python3 scripts/check_memory.py --heki target/release/heki
```

- CI fails when the peak heap of `heki check` (with and without an ownership contract) or of
  the `--watch` engine on a generated project exceeds its budget in the script.
- The peak heap is the most bytes heki holds at once, counted by the allocator of a
  `count-alloc` build (release builds leave the feature off). The counting build also runs its
  parallel work on the main thread alone, what `RAYON_NUM_THREADS=1` asks without the second
  thread whose timing would move the peak. No allocator cache, memory pressure, or thread timing
  changes the count: every run reads the same bytes, and one run per mode suffices. The total of
  the bytes allocated is printed next to it.
- Each budget is the peak on CI's Linux runner when it was set, plus 5 %. Another machine reads
  a few hundred bytes more or less, from the length of the generated project's path.
- A budget changes, down or up, only with the owner's decision. A change that would exceed one
  first finds what grew (below) and removes it where it lives; when the growth is the cost of
  facts a default rule needs, the measurements go to the owner.
- The generated project is shaped after what dominates heki's memory on real code bases: typed
  field writes reached through long chains of private helpers (with an ownership contract, every
  caller keeps the write sites they hand over, with their chains), effect chains below
  `async def` entry points, and many modules and classes (every file's check reads the project's
  declarations).

At release time (`release.yml` runs CI on the release commit) and on a manual CI run, a release
build also runs the three modes, and its peak memory as the system sees it, the lowest of three
runs, stays under a coarse budget: about 10 % above it when the budget was set. This peak also
holds what the heap count leaves out: the allocator's fragmentation, its mappings, and the code.
It is the resident set size on Linux and the physical footprint on macOS, each platform with its
own budgets.

```console
$ cargo build --release -p heki
$ python3 scripts/check_memory.py --heki target/release/heki --measure rss
```

To find what grew:

- the script prints each mode's peak and total heap bytes, so counting builds of two commits
  compare to the byte;
- a heap profiler (`dhat` in a scratch build, or heaptrack on Linux) shows the allocation sites
  live at the peak;
- `/usr/bin/time -l` (macOS) or `-v` (Linux) gives the peak memory of a run as the system sees
  it.

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
- A question an analysis cannot decide (a walk past its budget, a size the code does not fix, a
  premise nothing establishes) answers `heki_ir::walk::Answer::Open`, heki's one type for an
  answer that is established or unknown. Each use states what an open answer means to it, with
  `open_means` or a `match`: a rewrite takes it as "no rewrite", an exact report as "no finding",
  and a heuristic report states its own choice there.
- A `finally` body is built once per way out of the `try`. Anything made per copy, such as a
  class body, is made once.
- Dead code is modeled too. A loop after a `return` has only its back edge, and copies around it
  (`y = x; x = y`) can close a cycle. A walk along copies ends on such a cycle.
- A rule that follows exception edges, one from each call that can raise, runs a dataflow over
  blocks with one fact per tracked value. It never enumerates paths.
- A contract's targets are looked up from what an access names (the dotted suffixes of a
  function's names, its module and qualname, its class), never tried one by one: the cost grows
  with the sites, not with sites times targets, and the targets of every contract of a rule are
  indexed together.

The engines of `heki_ir::walk` apply these rules by construction, and every walk over IR ids,
the call graph, or the class hierarchy goes through them:

- `Memo` keeps one answer per key, a value and a question. `Closure` joins the facts of every
  node a node reaches, the members of a cycle sharing one. `reach` walks with a visited set.
  `fixpoint` joins the facts of nodes that read each other, and visits a node again only when a
  fact it reads grows, up to a visit limit.
- A `Budget` per owner (a rule run on one function, the summary of one function) counts the
  steps of all the owner's walks, and the weight of every fact they join, which bounds both the
  work of joining and the memory of the facts kept: 64 per IR unit, at least 4,096. Past it every
  walk of the owner answers open.
- `FnShape` holds the tables of one function that walks read instead of scanning it:
  definitions, readers, copy roots, predecessors, reverse postorder, dominators, and loops.
- `SharedMemo` keeps answers over the project declarations across functions, and replays what
  deciding each one read, so the language server's recorded dependencies stay complete. The
  other engines hold per-analysis state and are not `Sync`, so none can be stored in the shared
  `ProjectIndex`.

`crates/heki/tests/architecture_walks.rs` finds walks written by hand in the analysis crates:
recursion over IR ids (W1), worklists (W2), passes over every block until nothing changes (W3),
and the linear lookups `FnShape` answers from a table (W4). `crates/heki/tests/walks.allow` lists
the functions it still allows, each with its reason. The test fails on a finding the list does
not allow and on an entry that finds nothing any more, so the list only shrinks as walks move.

`heki check --walk-steps <path>` (hidden) writes the steps each owner's walks took per function as
JSON: growth across sizes of a shape is then checked without timing noise.

Each bound comes with a shape in `crates/heki/tests/e2e_analysis_bounds.rs` that is slow without it,
checked against a time limit: comparisons sharing an operand thirty deep, many `match` statements of
slicing or pass-through cases, `t1 = t0[t0]` chains in a loop, a loop with many `continue`s and
`isinstance` tests, a long chain of assignments a loop carries, many loops one after another, each
with a lazy initialization (the blocks each covers count toward a budget proportional to the
source), many waits on one future before many `await`s of it, task factories at the end of a long
chain of copies, a long chain of aliases of a class with an attribute store through the last one per
alias, a class with thousands of subclasses whose class constant as many functions read on an
instance, a strict `read-owners` target on such a class that as many functions read on an object of
unknown class, as many calls of a method of such a class that a call target naming no class names, a
module binding thousands of names each to an attribute of the one before, a module binding thousands
of names each to the one before, a method returning one of thousands of copies of an attribute
behind as many tests, a function making twenty thousand calls of eight operands each, a helper that
types nothing making thousands of strict accesses of one name that one caller calls as many times, a
helper that types nothing reading, calling, or writing one strict target name thousands of times,
which a caller calls as many times, many `is` tests each followed by a conditional expression on the
tested parameter, many branches on a long chain of copies of one `is` test, a long chain of returned
calls each handed the previous call's result as two of its arguments, a long chain of returned
aliases each appended to one list or to a new list of its own, a long chain of conditional
expressions each choosing between a call of the previous value and the value, one returned call
handed the last of a long chain of item reads as each of its arguments, a loop carrying a long chain
of assignments in reverse, a loop rotating aliases of many `self` containers, a chain of merges of
many `self` containers, a loop over a generator whose body makes many calls, loops whose bodies hold
many conditional blocks one after another, a function consuming a generator's result many times in a
row, a loop over a `zip` of many generator results whose body makes as many calls, a loop over a
`zip` of thousands of `self` containers whose body makes as many calls, one `zip` of thousands of
`self` containers stepped by as many `yield from`s, a generator handed on through thousands of
private wrappers, each returning the one before's result, an edit through the language server
changing every method of a class of thousands of methods, methods changing a typed dict of `self`
thousands of times, a method storing under a key after each of hundreds of tests that such a dict
holds the key, and a function reading attributes down a chain ten thousand deep (`x = x.child`),
many queue puts each after a branch that may build a new queue, a long chain of helpers handing a
queue on, named in the opposite order of the calls, a class whose thousands of queue attributes each
store the one before, with a put on each, and the path facts of a `try` body of thousands of calls
whose handler returns, of loops nested twenty deep with a call at each depth, of a thousand loops
one after another, and of a chain of hundreds of `and` tests of module constants, and the item
origins of a chain of thousands of wrappers, of a list filled by appends in a thousand loops, and of
a phi of sixty branches each choosing a different parameter, and thousands of `call-owners` targets,
in one contract or in as many contracts, and thousands of references to a `call-owners` target that
a long chain of phis carries to one `return`.

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
- Each rule's certainty (`exact` or `heuristic`) and activation (`Default`, `OptIn`, or
  `Contract`) are fields of its entry in `crates/heki_rules/src/lib.rs`. The generator marks the
  heuristic rules on the generated pages, copies each rule's measurement from
  [Rule Precision](precision.md) into its page, and fails when a hand-written table row says
  "heuristic" other than exactly when a rule it names is heuristic, when an exact rule lacks a
  certification there with no false claim in its census and holdout run, or when a rule's
  documentation has no "What a report states" section.
- The Docs workflow deploys the site to Cloudflare Pages (heki.koxudaxi.dev) once a published
  release is on PyPI (the newest release only), or when it is run by hand on `main`, so the site
  documents the released version. That job runs in the `docs-production` environment, which
  admits only `main` and release tags.
- A pull request from this repository gets a preview deployment of the same Pages project, on the
  branch `pr-<number>`. Both use the repository secrets `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID`.

## Releases

The Release Draft workflow (`.github/workflows/release-draft.yml`) keeps a draft GitHub release
of the next version. Publishing the draft runs `release.yml`, which builds and publishes heki, and
the Release Sync workflow (`.github/workflows/release-sync.yml`), which brings `main` to the
release. A draft is published by hand, and only while no Release Draft run is queued or running.

The release tag is the version (`v` + the version), so nothing is bumped before a release. Right
after checking out the tag, every `release.yml` job that builds writes the version into
`pyproject.toml`, `Cargo.toml` (`workspace.package.version` and the internal crates under
`workspace.dependencies`), and the workspace entries of `Cargo.lock`
(`scripts/set_release_versions.py`, which fails when a value it rewrites is not found exactly
once), and the release fails unless every wheel, the source distribution, and the executable
declare it. Nothing is committed: `main` declares the newest release once the sync pull request
is merged.

### How a draft accumulates

After each push to `main`, the workflow takes the pull requests merged at the commits the push
added (several when a stack merges at once). For each one, Claude reads the merged diff and
decides whether it breaks something the newest published release has. A change is breaking
when:

- existing code gets new reports from the rules enabled by default: a new rule enabled by
  default, a rule turned on by default, or wider detection in a rule enabled by default;
- a rule enabled by default becomes opt-in or is removed;
- a rule's default severity changes;
- a rule code or name changes;
- configuration keys or their meaning, command-line options, or exit codes change, including how
  `requires-python` selects the target Python version;
- fields of the JSON or SARIF output are removed or change meaning;
- the language server behavior that editors rely on changes;
- the format of `--fix` rewrites changes;
- the supported Python versions or platforms change.

These are not breaking: a change that only removes false reports, in a default or an opt-in rule
(a fixed false positive), even though it narrows what the rule detects; new opt-in rules; new
optional keys and new output fields; and any change to a feature that `docs/experimental.md`
lists. The analysis context that Claude reads holds that page as the trusted checkout of `main`
has it (while the page does not exist, no feature is experimental), and Claude describes each
change to an experimental feature separately. The workflow then:

- adds the breaking changes to the draft's `## Breaking Changes` section, under one heading per
  kind, the changes to experimental features to `## Experimental`, and a line
  `* <title> (#<number>)` to `## Changes`, except for pull requests labeled `maintenance` or
  `dependencies`; every item ends with `(#<number>)`;
- labels a breaking pull request `breaking-change`, comments the result on every pull request,
  and labels each `breaking-change-analyzed`, so that a re-run skips it. A pull request analyzed
  again keeps one comment, which is updated, and loses `breaking-change` when it is no longer
  found breaking.

The draft's tag is the version after the newest published release: a breaking change bumps the
minor version (0.1.x to 0.2.0), anything else the patch, and manifests that already name a
later version win (a pull request that sets a later version in them picks a larger bump). Text
added to the draft by hand is kept. Pull requests that change only tests, documentation,
development scripts, or the CI workflows skip Claude and count as not breaking. While no release is published, the workflow does nothing: the notes of the first
release come from `CHANGELOG.md`.

### Publishing a release

The certifications of [Rule Precision](precision.md) must hold for the release candidate first
(release gate 3 in `ARCHITECTURE.md`): a corpus run, and a check of every report it adds or
changes in an exact rule.

1. Check that the first unreleased section of `CHANGELOG.md` is headed
   `## [<version>] - Unreleased`, with the version of the draft's tag: the sync dates that
   section.
2. Wait until no Release Draft run is queued or in progress
   (`gh run list --workflow release-draft.yml`), then reload the draft and publish it on GitHub.
   GitHub cannot make a draft update conditional on the release still being a draft, so a run
   that updates the draft while it is published rewrites the published tag, title, and notes
   (the run then warns), and a draft page loaded before the run's update publishes without it.
3. The Release Sync workflow opens the pull request `chore: sync release metadata for <version>`
   from the branch `release/sync-<version>`, labeled `maintenance` to keep it out of the next
   draft's changes. It writes the version into the manifests and `Cargo.lock`, renames the
   section heading to `## [<version>] - <publish date>` (the UTC date), and makes the
   `[<version>]` compare link end at the release tag instead of `HEAD` (a missing link compares
   the previous release with the tag). When a section is headed `[Unreleased]`, its link compares
   the tag with `HEAD`. The entries stay as they were written.
4. GitHub creates the CI runs of a pull request that a workflow opens with `GITHUB_TOKEN` in an
   approval-required state, and the checks of a run started with `workflow_dispatch` do not
   count for the required checks of `main`. Close and reopen the pull request to start CI
   (`gh pr close <number> && gh pr reopen <number>`), or select **Approve workflows to run** in
   its merge box, and merge it once CI passes.

When the sync fails, for example because `CHANGELOG.md` has no section for the version, fix
`main` in a pull request and run it again with
`gh workflow run release-sync.yml --ref main -f tag=v<version>`. A run whose changes `main`
already has opens nothing, and a run for an open sync pull request updates it. A pre-release,
and a release older than the version `main` declares (a backport published late), leave `main`
as it is.

The builds can be tried without publishing:
`gh workflow run release.yml --ref <branch> -f version=<version>` builds and checks the
distributions with that version (it cannot be combined with `publish`), and without `version` they
keep the version of the committed manifests.

One-time setup, done for this repository: **Settings > Actions > General > Workflow permissions >
Allow GitHub Actions to create and approve pull requests**, which lets the sync open its pull
request with `GITHUB_TOKEN`. The sync needs no secret, and the ruleset of `main` is unchanged.

### Replays and dry runs

```console
$ gh workflow run release-draft.yml --ref main -f pr_number=123
$ gh workflow run release-draft.yml --ref <branch> -f pr_number=123 -f dry_run=true
```

- A replay on `main` analyzes one merged pull request again, for example after a failed run. A
  pull request labeled `breaking-change-analyzed` is skipped: remove the label first. One that a
  published release already holds is skipped too, so that its changes are not listed again in
  the next draft; a dry run still analyzes it.
- Up to 100 runs wait for the one in progress, in order. GitHub cancels a run that arrives while
  100 are waiting, and a later run does not take over its pull requests. A canceled run shows in
  `gh run list --workflow release-draft.yml --status cancelled`. Its pull requests are among those
  merged after the first published release that still lack `breaking-change-analyzed`. List them
  with `gh pr list --state merged --limit 1000 --search "-label:breaking-change-analyzed
  merged:>=<first-release-date>"` (`gh pr list` shows 30 without `--limit`, and a search returns
  at most 1,000, so split the date range when it lists 1,000), and replay each of them, in merge
  order.
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
