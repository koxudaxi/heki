# Adopting heki in a Code Base

An existing code base usually has findings you cannot fix at once. The
[starter recipes](#starter-recipes) below are the common setups as blocks to copy. The rest of
this page shows how to adopt heki step by step:

1. Record today's findings in a [Ratchet baseline](#ratchet-baselines), so heki fails only on
   new ones.
2. Run it in [pre-commit](#pre-commit) and [continuous integration](#continuous-integration).
3. [Roll out more rules](#rolling-out-rules): contracts for your shared state, and the opt-in
   rules where they matter.
4. For a library, save [effect snapshots](#effect-snapshots-mut103) of its public functions.

To have a coding agent do this, ask it to adopt heki: the [Agent Skill](coding-agents.md#agent-skill-experimental)
carries a playbook that proposes the first one or two contracts, writes `heki.toml` with a
`CODEOWNERS` line, the baseline, the hook and the CI job, and runs the whole project before it
finishes.

## Starter recipes

Each recipe is one change to copy, with the version pinned: an upgrade is then a change of the
pins, reviewed like any other (`pre-commit autoupdate` moves `rev`). Replace `app` with your
package.

### Add heki to an existing project in one pull request

Record today's findings, so that only new ones fail, and commit `heki-baseline.toml` with the
configuration, the hook, and the CI step:

```console
$ uvx heki==0.11.0 check --baseline heki-baseline.toml --update-baseline
```

```toml
# pyproject.toml
[tool.heki]
baseline = "heki-baseline.toml"
```

```yaml
# .pre-commit-config.yaml
repos:
  - repo: https://github.com/koxudaxi/heki-pre-commit
    rev: v0.11.0
    hooks:
      - id: heki   # heki check --touched on the staged files
```

```yaml
# A step of the CI job, with uv installed
- run: uvx heki==0.11.0 check
```

The hook shows the reports in and through the staged files, and every regression of the
baseline; the CI step checks the whole project. Both fail on new exact errors.

### An async web or database service

`EFF101` (a blocking call that holds up the event loop), `EFF102` (a new HTTP or database
client on every iteration of a loop), `EFF103` (a query on every iteration of a loop), and
`EFF109` (a lock taken again while it is held) are exact and run by default.
Add the checks that fit the service:

```toml
# pyproject.toml
[tool.heki.lint]
extend-select = [
  "EFF105",  # blocking file I/O reached from an async def
  "EFF106",  # an await or network I/O while a threading lock or a transaction is held
  "EFF107",  # an asyncio task bound to a name and then lost
]
```

| Rule | Certainty | Add it when the service |
| ---- | --------- | ----------------------- |
| [`EFF105`](rules/async-blocking-fs.md) | exact | reads or writes files, templates, or static assets in `async def` handlers |
| [`EFF106`](rules/lock-or-tx-held-across-io.md) | heuristic | holds a `threading` lock or a database transaction around network I/O or an `await` |
| [`EFF107`](rules/unretained-async-task.md) | heuristic | starts background work with `asyncio.create_task` |
| [`EFF110`](rules/exclusive-object-fanout.md) | heuristic, experimental | shares a connection, a stream, or a WebSocket between tasks started with `asyncio.gather` or a task group |
| [`EFF115`](rules/async-def-as-sync-callback.md) | heuristic, experimental | hands `async def` functions to threads, executors, or signal handlers |
| [`EFF117`](rules/unlocked-write-of-guarded-state.md) | heuristic | changes state its threads share without the lock its other writers hold; it is not opt-in and runs with `--heuristic` |

Their reports can be wrong about the code: they are printed, and fail the run only with
`fail-on = "warning"`. A [contract](configuration.md#contracts) can make its own reports fail it with
`fail = true`, and `fail = ["IMP101"]` in `[tool.heki.lint]` does so for the reports of the rules it lists. Before adding a rule, [`heki recommend`](recommend.md) counts the reports
it would add on your code ("Rules worth enabling here"). `relax-async-in-tests`, on by default,
leaves `EFF101`, `EFF105`, and `EFF106` out of test code.

Two contracts keep the rest of the service in line: handlers that never block, and database
writes that only the repository layer makes. `fail = true` makes their reports fail the run.

```toml
# heki.toml
[[effect-boundaries]]
name = "handlers-do-not-block"
scopes = ["app.web.*"]
forbid = ["blocking"]
message = "await an async client, or move the call to app.repo and run it in a thread"
fail = true

[[call-owners]]
name = "db-writes-in-repo"
targets = ["app.db.Session.add", "app.db.Session.commit"]
allowed-callers = ["app.repo.*"]
message = "write through a function of app.repo"
fail = true
```

The first reports a `time.sleep` two calls below an `async def` in `app.web` (`EFF201`). The
second reports a handler that calls `session.add` or `session.commit` itself (`MUT204`). Use
`write-owners` instead when the rule is about a field, not a call:
[State Ownership](ownership.md#which-contract-do-i-need).

### A layered application

Keep the domain free of the web and infrastructure layers, and keep a shared settings object
from being changed in place:

```toml
# heki.toml
[[import-boundaries]]   # experimental
name = "domain-is-a-leaf"
importers = ["app.domain", "app.domain.*"]
forbidden-imports = ["app.web", "app.web.*", "app.infra", "app.infra.*"]
message = "pass what the domain needs in as an argument"
fail = true

[[readonly-returns]]
name = "shared-settings"
producers = ["app.settings.get_settings"]
deep = true
fail = true
```

A module of `app.domain` that imports `app.infra` is reported as `IMP201`, and a view that
assigns to `get_settings().debug` or appends to `get_settings().hosts` as `MUT202`. See
[Imports](imports.md#architecture-import-boundaries) and
[State Ownership](ownership.md#which-contract-do-i-need).

### Walls for a team or a coding agent

Declare the architecture as contracts, make their reports fail the run with `fail = true`, and give
the file an owner, so that a wall moves only in a reviewed change:

```toml
# heki.toml
baseline = "heki-baseline.toml"

[[write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
message = "change Order.items through app.store"
fail = true   # contract reports are heuristic: this makes them fail the run

[[effect-boundaries]]
name = "pure-domain"
scopes = ["app.domain.*"]
forbid = ["blocking"]
fail = true

[[import-boundaries]]   # experimental
name = "domain-without-web"
importers = ["app.domain", "app.domain.*"]
forbidden-imports = ["app.web", "app.web.*"]
fail = true
```

```text
# .github/CODEOWNERS
/heki.toml           @your-org/maintainers
/heki-baseline.toml  @your-org/maintainers
/.github/CODEOWNERS  @your-org/maintainers
```

- heki reads the first of `heki.toml`, `.heki.toml`, and `pyproject.toml` it finds at the
  project root, without merging them: move every `[tool.heki]` setting into `heki.toml`.
- `fail = true` makes the reports of that contract fail the run and leaves every other warning
  and heuristic report advisory. `fail-on = "warning"` would fail on all of them, which a project
  that wants that can still set. Record today's findings in the baseline first
  (`uvx heki==0.11.0 check --update-baseline`), so that only new ones fail.
- The `message` is the help of every report of the contract: write it as an instruction.
- A baseline does not stop a new `# heki: ignore[...]` or `# noqa` from hiding a report. In
  review and in CI, write `heki check --report-suppressed head.json` for the pull request and for
  its base and compare them ([Auditing inline suppressions](#auditing-inline-suppressions)).
  `CODEOWNERS` covers the contract and baseline files, and this comparison covers the comments.
- For Claude Code, add the [Stop hook](coding-agents.md#a-stop-hook-for-claude-code): it runs
  `heki check`, with the baseline `heki.toml` names, when the agent is about to end its turn,
  and sends a failing report back to it.

[Contracts](configuration.md#contracts) has every key, and
[Coding Agents](coding-agents.md#architecture-rules-with-agents) the instructions for the agent.
[The whole setup, end to end](coding-agents.md#the-whole-setup-end-to-end) adds the GitHub Actions
workflow that compares a pull request with its base branch, the Stop hook, and the skill.

## Ratchet baselines

A Ratchet baseline records the findings you have today and fails only on new ones:

```console
$ heki check --baseline heki-baseline.toml --update-baseline   # record today's findings
$ heki check --baseline heki-baseline.toml                     # fail on regressions only
```

Set `baseline = "heki-baseline.toml"` in the [configuration](configuration.md#top-level-settings)
to use it without the flag, also in `--watch` and the language server.

Entries are keyed by file, function, rule, and target, not by line number, so unrelated edits and
reformatting do not break the baseline ([import cycle groups](#import-cycle-groups) are keyed by
their modules):

```toml
# heki Ratchet baseline: counts may only shrink (`heki check --update-baseline`).
version = 3

[[entries]]
path = "app/api.py"
qualname = "get_user"
rule_code = "EFF101"
target = "requests.get"
count = 1

[[entries]]
path = "app/jobs.py"
qualname = "refresh"
rule_code = "EFF101"
target = "time.sleep"
count = 2
heuristic = 1
```

The recorded set can only shrink:

| Situation | `heki check` | `heki check --update-baseline` |
| --------- | ------------ | ------------------------------ |
| No baseline file yet | an error (exit code 2) | records today's findings |
| A finding was fixed | a note that the entry can shrink; with [`report-unused-suppressions`](configuration.md#unused-suppressions), an entry that matches nothing is an error (exit code 1) | records the smaller entry |
| An entry grew, a recorded heuristic report is exact now, or a new finding appeared | reports it, with a note on the allowed and found counts (exit code 1 for an exact error, and for a warning or a heuristic report with [`fail-on = "warning"`](configuration.md#rule-selection-toolhekilint)) | reports it the same way and leaves the file as it is (exit code 1) |
| A recorded exact report is heuristic now | a note that the entry can shrink | records it as heuristic |
| A file could not be analyzed (a syntax error, an unreadable file) | reports the error and the other files' findings (exit code 2); a run over given files fails only on an error in one of them | writes nothing, so the entries of that file are not lost (exit code 2) |

To record new findings on purpose, delete the baseline and record it again.

A run over part of the project (`heki check pkg_a --baseline heki-baseline.toml`) compares and
updates only the entries of the files below its paths: the entries of the other files stay as
they are, since the run did not analyze them. A baseline written before version 3 is updated
whole first. `--update-baseline` takes directories: a file given to `heki check` selects what is
shown, and a baseline records every report of what a run covers.

### What an entry allows

Each entry counts violation sites, not reports (`sites` in JSON output):

- a contract report at the caller of a private helper stands for every write, read, or call
  inside the helper;
- an effect boundary report stands for every forbidden call or write its scope function reaches.

So a second violation behind a recorded report is a regression too.

`heuristic` says how many of the sites were [heuristic](precision.md) reports, and is left out
when none were. A heuristic report states the weaker claim of the same premises as an exact one,
so an entry allows the claims it recorded and no stronger one: a run regresses when it finds more
sites than `count`, or more exact sites than `count` less `heuristic`. In the example, `refresh`
may have two reports of `EFF101` at `time.sleep`, and one of them may be exact. A recorded
heuristic report that heki proves after a change of the code, or after an upgrade in which heki
proves more, is a regression once: fix it, or record the baseline anew. An exact report that turns
heuristic passes, and `--update-baseline` records it as heuristic. A rule with two tiers keys both
in the same entry: `EFF101` and `EFF103` key a report by the last call of the shortest call chain
to the blocking call or the query, also where the exact report names the call every
implementation runs (a file of version 2 keys it by the call the exact report names).

Older files keep their meaning until `--update-baseline` rewrites them with `version = 3`, which
heki 0.5.0 and earlier cannot read:

- A baseline with `version = 2` does not record which sites were heuristic: any of them may be
  exact, as before. `heki check` says so in a note.
- A baseline without a `version` (or with `version = 1`) was written before heki counted sites,
  and counts reports. `heki check` says so in a note, and a second site behind a recorded report
  is no regression.
- `--update-baseline` compares in the file's own terms first, so nothing that grew is taken in,
  then rewrites the file.

A version this heki does not know is an error, and so is a file listing one entry twice (a hand
merge of two baselines).

### Import names

An [`IMP201`](rules/import-boundary-violation.md) report of `from m import n`, where `n` is a name
of `m`, has the target `m.n`, and its entry holds the name, so another name of the same module is a
new report. An entry of the module, as heki 0.5.0 and earlier wrote it, keeps covering the names of
its module that have no entry of their own, with the same count, and `--update-baseline` rewrites
it per name. Only a heki that keys these reports by name reads the rewritten entries: the file
has `version = 3`, which heki 0.5.0 and earlier cannot read, so every heki that reads the
baseline must be newer before a rewritten baseline is committed.

### Import cycle groups

An [`IMP101`](rules/import-cycle.md) entry records a group of modules that import each other, and
names no file or function:

```toml
[[entries]]
rule_code = "IMP101"
target = "pkg.a, pkg.b, pkg.c"
count = 1
```

A group is covered when one entry holds all of its modules, whichever import closes the cycle
its report shows and whichever function holds that import:

- A group that lost modules, or split into several groups, is covered: a note says the entry
  can shrink to the groups there are now, and `--update-baseline` records them.
- A group with a module no entry of it holds is a regression, and so is a group joining two
  recorded groups: the note names the groups it joins and the modules none of them holds. A
  renamed module is such a module.
- An entry matches no report only when none of its modules is in a group.

A group spans files, so a run decides its entry only when it covers every file of the group: then
the entry is dropped when no group is left within it, or replaced by the groups there are. A run
over part of the project keeps the entries of the other groups, and records every group it
reports. An entry is reported as unused only when the run checked every file of its group and
`IMP101` runs on every one of them (`per-file-ignores` turning it off on one file leaves it
unjudged). An entry written by heki 0.4.0 or earlier names the file and function of the import its
group was reported at; heki reads it as the entry of its group, and `--update-baseline` rewrites
it. heki 0.4.0 and earlier cannot read a group entry written without them, so every heki that reads
the baseline must be newer than 0.4.0 before a rewritten baseline is committed.

## pre-commit

The hook passes the staged Python and TOML files to `heki check --touched`, which passes over a
TOML file that is not part of the configuration. heki follows calls across files, so it still
analyzes the whole project, but it shows and fails on only the reports in those files and the
reports whose call chain reaches one of them: after a change to `app/users.py`, the `EFF101` in
`app/api.py` that reaches the change through two helpers is shown, and old findings in files the
commit left alone are not. A project can run the hook from its first commit, without a baseline.

That is all a run over touched files can promise. A change can also make a report appear in a
file the commit left alone without a call chain through the change: a module that re-exports
another function, or a return annotation another module relied on. Whether a change added such a
report cannot be told from the code after it, so:

- Without a baseline, the run ends with a note counting the reports that would fail it and are
  not shown, and when it passes, its last line says what it looked at (`No reports in or through
  2 touched file(s) (40 file(s) analyzed).`), not that all checks passed.
- With a baseline, every regression is shown and fails the run, wherever it is (see below).
- A configuration file can change any report, so naming one (`pyproject.toml`, `heki.toml`,
  `.heki.toml`, or `hatch.toml` at the root, a file the configuration
  [includes](configuration.md#several-configuration-files), the summary snapshot, the baseline)
  shows every report, as a run without `--touched` does: a contract added to `heki.toml` is checked in the
  whole project.

Use `--touched` while editing and in the hook, and run the whole project, with the baseline, before
finishing a change and in continuous integration. A local hook may also pass the files as paths
(`entry: heki check`): a file given to `heki check` is checked in its project, as with `--touched`.
With `--fix`, the hook rewrites only the staged Python files: the fix of a shown report in another
file is left, and the run says how many were.

```yaml
# .pre-commit-config.yaml
repos:
  - repo: https://github.com/koxudaxi/heki-pre-commit
    rev: v0.11.0
    hooks:
      - id: heki
```

[heki-pre-commit](https://github.com/koxudaxi/heki-pre-commit) installs heki from its prebuilt PyPI
wheel (the [platforms](installation.md) heki publishes wheels for), so it compiles nothing and needs
no Rust toolchain. Its tags follow heki releases, so `pre-commit autoupdate` moves `rev` to the
latest release.

A local hook can run any version of the PyPI wheel, also without a compile or a Rust toolchain:

=== "uv"

    ```yaml
    # .pre-commit-config.yaml
    repos:
      - repo: local
        hooks:
          - id: heki
            name: heki
            entry: uvx heki==0.11.0 check --touched
            language: system   # needs uv on PATH
            types_or: [python, toml]
            files: '(\.py|^(pyproject|heki|\.heki)\.toml)$'
            pass_filenames: true
            require_serial: true
            verbose: true      # show warnings: they do not fail the hook
    ```

=== "Without uv"

    ```yaml
    # .pre-commit-config.yaml
    repos:
      - repo: local
        hooks:
          - id: heki
            name: heki
            entry: heki check --touched
            language: python
            additional_dependencies: [heki==0.11.0]
            types_or: [python, toml]
            files: '(\.py|^(pyproject|heki|\.heki)\.toml)$'
            pass_filenames: true
            require_serial: true
            verbose: true      # show warnings: they do not fail the hook
    ```

pre-commit appends the files after the hook's `args`, so `--touched` comes last: options for the
`heki` hook go before it (`args: [--fail-on, warning, --touched]`). To check the whole project on
every commit instead, set `args: []` and `pass_filenames: false` on the hook, or drop `--touched`
from a local hook's `entry` and set `pass_filenames: false`.

The hook fails on exact errors and passes with warnings and heuristic reports, which it prints. To
make them fail it too, set `fail-on = "warning"` in `[tool.heki.lint]`, or add `--fail-on warning`
before `--touched`.
With a baseline configured, the hook fails only on new findings. A baseline entry counts every
report of its key, so a run with `--touched` compares all of them and shows every regression: a
grown entry fails the commit wherever its reports are, also in a file the commit left alone, since
the commit added the report. It judges the stale entries of the touched files and of the reports
that touch them only: a stale entry elsewhere is left to a run over the whole project.

??? note "Building the hook from source"

    Only a platform without a wheel, or a development version of heki, needs this. Point `repo` at
    `https://github.com/koxudaxi/heki` with a release `rev`. pre-commit then installs the repository
    as a Python package, which maturin compiles with cargo (Rust 1.99 or newer; when cargo is not on
    `PATH`, the build installs Rust into a cache directory first). That takes a few minutes, and
    pre-commit keeps the installed hook for later runs.

## Continuous integration

A run fails (exit code 1) on an exact report of an `error` rule: a bug that can be critical, such
as a call that blocks the event loop (`EFF101`). Warnings (slower code, or a bug that is rarely
harmful) and heuristic reports, which can be wrong about the code, are reported without failing
it, whatever their rule's severity: a contract report, heuristic today, is one of them.
`fail = true` on a contract table makes the reports of that contract fail the run, and
`fail-on = "warning"` in `[tool.heki.lint]` (or `--fail-on warning`) makes every one of them fail it. A
code base with errors today records them in a [Ratchet baseline](#ratchet-baselines) first, so
that the job fails only on new ones; [`heki recommend`](recommend.md) says how many reports fail
the run now.

=== "GitHub Actions annotations"

    ```yaml
    - run: uvx heki==0.11.0 check --output-format github
    ```

=== "Code scanning (SARIF)"

    ```yaml
    # The job needs `permissions: security-events: write`.
    - run: uvx heki==0.11.0 check --output-format sarif > heki.sarif || test "$?" -eq 1
    - uses: github/codeql-action/upload-sarif@v4
      with:
        sarif_file: heki.sarif
    ```

Each annotation is one workflow command at the reported location: its title names the rule
(`heki EFF101`, `heki EFF101 (heuristic)` for a heuristic report), and its body is the message followed by one
`related: <path>:<line>:<col>: <message>` line per related location (the hops of a call chain, in order).

The level of an annotation says whether its report fails the run. A report that fails it keeps
its severity, `error` or `warning`; one that does not is a `warning`, so an annotation reads as
a failure only when the step fails on it. Warnings and heuristic reports do not fail the step:
by default a heuristic report of an `error` rule is a `warning` annotation. `fail-on =
"warning"` in `[tool.heki.lint]` (or `--fail-on warning`) makes them fail it, as in pre-commit,
and the heuristic `error` is then an `error` annotation; so are the reports of a contract with
[`fail = true`](configuration.md#failing-on-a-contract) and of a rule
[`lint.fail`](configuration.md#rule-selection-toolhekilint) names. The JSON, SARIF, and text
formats give each report its severity.

In GitHub Actions (`GITHUB_ACTIONS=true`), `heki check` writes these annotations without
`--output-format github`. To keep the text output there, pass `--output-format full`, or set
`HEKI_OUTPUT_FORMAT` (any format) in the job's `env`: it applies whenever the option is not given.

One run is enough: the annotations step fails the job on what fails the run. A run prints one
output format, so a job that also keeps code-scanning alerts runs heki twice: the annotations
step is the one that fails the job, and the SARIF step, which keeps the step green on findings,
uploads them.

On a pull request, a project with old findings can report only what the change touches, as the
pre-commit hook does (the checkout needs `fetch-depth: 0` for the base branch):

```yaml
- run: git diff -z --name-only --diff-filter=d "origin/$BASE...HEAD" -- '*.py' pyproject.toml heki.toml .heki.toml | xargs -0 -r uvx heki==0.11.0 check --touched
  env:
    BASE: ${{ github.base_ref }}
```

`--diff-filter=d` leaves out deleted files, and `xargs -r` skips the run when no Python file
changed. A path that names no analyzed file (a typo, a file outside the analyzed directories, an
excluded one) is named in a warning, and the run goes on with the others. With
`--unmatched-paths error` (or [`unmatched-paths = "error"`](configuration.md#rule-selection-toolhekilint)
in `[tool.heki.lint]`), it stops the run with exit code 2 instead, so a wrong path fails the job
rather than passing it on nothing.

With SARIF:

- findings become code-scanning alerts, warnings included, and keep the step green (exit code
  `1` when one fails the run);
- a configuration, read, or syntax error (exit code `2`) still fails the step; a file heki could
  not read or parse is an `error` notification of the run
  (`invocations[0].toolExecutionNotifications`, with `executionSuccessful: false`), and the
  results of the other files are kept;
- results carry the message, the location, the call chain (`relatedLocations`), and the help
  (`properties.help`; SARIF's `rules[].help` is one text per rule, and a report's help can name
  the object it is about).

For custom tooling, `--output-format json` gives every field: see
[Coding Agents](coding-agents.md#after-an-edit-heki-check).

### Auditing inline suppressions

A baseline, `report-unused-suppressions`, and `require-suppression-codes` do not stop a new
`# heki: ignore[EFF101]`, `# noqa: EFF101`, or bare `# noqa` from hiding a report, and the run
still exits 0. `--report-suppressed <PATH>` writes the reports that inline suppressions hid to a
JSON file, next to any `--output-format`:

```json
{
  "schema_version": 1,
  "suppressed": [
    {
      "code": "EFF101",
      "file": "app/handlers.py",
      "function": "fetch",
      "target": "time.sleep",
      "line": 12,
      "column": 5,
      "directive": {"kind": "heki-ignore", "file": "app/handlers.py", "line": 12, "column": 20}
    }
  ]
}
```

- `file`, `function`, `code`, and `target` are the key of the report in a
  [baseline](#ratchet-baselines), so a change of line numbers does not change what a CI job
  compares. Compare the entries as a multiset of those four fields: one removed entry does not
  offset an added one.
- `directive.kind` is `heki-ignore`, `heki-ignore-bare`, `noqa-code`, or `noqa-bare`. When several
  suppressions on a line hide the same report, the first one in the source is named.
- Only reports that a suppression hid are listed. A `# noqa: F401` that Ruff reads hides no heki
  report, and a suppression that hid nothing is not listed (`report-unused-suppressions`
  reports it).
- The file covers what the run shows: with `--touched` or a file in PATHS, the reports of those
  files, and a baseline changes nothing in it. It is the same bytes on every run of the same
  code, and the command exits with 2 when the file cannot be written.

A CI job can run the check on the base branch and on the change, and fail on an entry the change
adds. Check the base out from its protected ref, so that the change cannot alter the comparison:

```yaml
- run: |
    keys() { jq -r '.suppressed[] | [.file, .function, .code, .target] | @tsv' "$1" | sort; }
    uvx heki check --report-suppressed head.json || test "$?" -eq 1
    git worktree add ../base "origin/$BASE"
    (cd ../base && uvx heki check --report-suppressed "$GITHUB_WORKSPACE/base.json" || test "$?" -eq 1)
    # Sorted lines keep a repeated key apart, so one removed entry offsets no added one.
    new=$(diff <(keys base.json) <(keys head.json) | grep '^>' || true)
    test -z "$new" || { echo "New inline suppressions:"; echo "$new"; exit 1; }
  shell: bash
  env:
    BASE: ${{ github.base_ref }}
```

The configuration, the baseline, and the CI files that decide this (`pyproject.toml` or
`heki.toml`, the baseline file, and `.github/workflows/`) belong in `CODEOWNERS`:
the audit file shows what suppressions hide, and a weakened configuration or baseline is the
code owners' to review.

## Stability on the way to 1.0

On the way to 1.0, heki keeps these stable: the exit codes `0`, `1`, and `2`, the exact rules that
run by default, the format of the Ratchet baseline, the contract tables `write-owners`,
`read-owners`, `call-owners`, `readonly-returns`, `invalidation`, and `effect-boundaries` with
`fail = true` and `[lint] fail = [...]`, and the `json`, `sarif`, and `github` output formats. A
change to one of them is listed under "Breaking changes" in the release notes. The experimental
features can change in any release: `heki recommend`, `heki inspect`, `heki skill`,
`--generate-prompt`, and the rules marked experimental
([Experimental Features](experimental.md) has the list).

## Rolling out rules

1. Run [`heki recommend`](recommend.md) (experimental): it says which rules would report on
   the project, what heki could not see, and the contracts the code already keeps, with the keys
   to add. It writes no file.
2. Start with the default rules and a baseline, or `--select` one family at a time. Only the
   exact rules that are not opt-in run by default; add the heuristic ones with
   `lint.heuristic = true` (or `--heuristic`), or name the ones you want in `extend-select`.
   Their reports can be wrong about the code, and every output labels them `heuristic`
   ([Rule Precision](precision.md)).
3. Declare [contracts](configuration.md#contracts) for the shared state and the operations you
   care about, and effect boundaries for the code that must stay pure. `MUT201`, `MUT203`,
   `MUT204`, `MUT208`, and `EFF201` report nothing until a contract exists.
4. Enable the opt-in rules where they matter:

    | Rule | Certainty | Turn it on for |
    | ---- | --------- | -------------- |
    | [`EFF105`](rules/async-blocking-fs.md) | exact | async services |
    | [`EFF107`](rules/unretained-async-task.md) | heuristic | async services |
    | [`EFF106`](rules/lock-or-tx-held-across-io.md) | heuristic | threaded code, and code that holds database transactions |
    | [`MUT103`](rules/summary-effect-regression.md) | heuristic | library APIs (with [effect snapshots](#effect-snapshots-mut103)) |
    | [`MUT106`](rules/iterator-double-consumption.md) | heuristic | code that passes iterators and generators around |
    | [`MUT108`](rules/readonly-type-mutation.md) | heuristic | code that relies on frozen dataclasses and `ReadOnly` fields |
    | `OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130` | exact | loops over large inputs; `--include-size-dependent` runs all of them except `OPT103` (which has no rewrite), and `--fix` then applies the rewrites |

5. Use `per-file-ignores` rather than broad `ignore` lists, so new code stays covered.

### From heki 0.1

heki 0.1 ran every rule that is not opt-in by default. Since rules have a certainty, only the
exact ones that are not opt-in do ([Getting Started](getting-started.md#run-it) lists them).

- `lint.heuristic = true` restores the rules of 0.1, now labelled `heuristic` in every output,
  except the size-dependent `OPT` rules, which are opt-in since 0.3.0: name them in
  `extend-select` or `--select`, or pass `--include-size-dependent` for all but `OPT103` (`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`).
- A baseline keeps its entries: their keys (file, function, rule, target) do not change, with one
  exception. `MUT109` is now `MUT208` and the old code is no alias, so an entry with rule `MUT109`
  no longer matches: its report counts as a regression. Change `rule_code = "MUT109"` to
  `"MUT208"` in those entries, or record the baseline anew.
- `heki check --fix` applies the proven fixes of `OPT101` and `OPT102` by default, as in 0.1.

### From heki 0.2 and 0.3

`EFF111` and `EFF113` are no longer codes of their own: their reports are the heuristic reports
of `EFF101` and `EFF103`, labelled `heuristic` and shown, as before, with `lint.heuristic = true`
or where the code is named. The old codes and names are not kept.

- Write `EFF101` for `EFF111` (`async-possibly-blocking-call`) and `EFF103` for `EFF113`
  (`loop-possible-db-query`) in `select`, `extend-select`, `ignore`, `per-file-ignores`,
  `severity`, and suppression comments. In the configuration the old codes are unknown rules
  (exit code 2); a comment naming one suppresses nothing.
- A baseline entry with `rule_code = "EFF111"` or `"EFF113"` no longer matches: its reports count
  as regressions. Change the code to `"EFF101"` or `"EFF103"` (adding up the counts of two entries
  that then have the same file, function, and target), or record the baseline anew.

### From heki 0.5 and earlier

- `--update-baseline` writes `version = 3`, which heki 0.5.0 and earlier refuse: update heki
  everywhere the baseline is read (the pre-commit `rev`, CI) before committing a rewritten
  baseline. A file of version 2 is read as before, with a note ([What an entry
  allows](#what-an-entry-allows)).
- Once rewritten, an entry recorded for a heuristic report no longer allows an exact one: a
  report that heki proves is a regression once.
- An exact `EFF101` or `EFF103` report is keyed by the last call of the shortest call chain, as a
  heuristic one is, in a file of version 3. A file of version 2 keeps the call the report names,
  so its entries still match, and `--update-baseline` rewrites them with the new key.

## Effect snapshots (`MUT103`)

For a library, a change in what a public function *does* is worth reviewing even when no rule
fires: it starts mutating an argument, or doing network I/O. Save the effect summaries of the
public functions and check against them:

```console
$ heki snapshot --write                        # writes .heki-summary-snapshot.json
$ heki check --extend-select MUT103            # reports functions that gained effects
```

1. Commit `.heki-summary-snapshot.json`.
2. When a new effect is intended, refresh the snapshot in the same pull request: the diff of the
   JSON file shows reviewers what changed.

A run that cannot analyze every file (a syntax error, an unreadable file) does not write the
snapshot: it reports the errors and exits with code 2, so the entries of those files are not
lost.

### Comparing with the protected base

Instead of committing the snapshot, a pull request can be compared with a snapshot of
its protected base branch. Generate `base.json` on the base with the same heki version, the same
knowledge bases, the same `--target-version`, and the same configuration as the check on the
pull request, then check the whole project against it:

```console
$ git switch --detach origin/main
$ heki snapshot > ../base.json
$ git switch --detach "$PR_HEAD"
$ heki check --select MUT103 --fail-on warning --snapshot ../base.json
```

`--snapshot PATH` replaces `.heki-summary-snapshot.json` at the project root. A path that does
not exist, a file that is not a snapshot, a snapshot of an unsupported version, and a run in
which `MUT103` is not enabled all exit with code 2: the check never passes without having
compared. A snapshot at the root is ignored while `--snapshot` is given. Run the whole project,
not `--touched` on the files of the pull request: a public function gains an effect through a
private helper in another module, and the report is on the public function.
