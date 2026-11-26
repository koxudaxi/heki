# Adopting heki in a Code Base

An existing code base usually has findings you cannot fix at once. This page shows how to adopt
heki step by step:

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

The hook passes the staged Python files to `heki check --touched`. heki follows calls across files,
so it still analyzes the whole project, but it shows and fails on only the reports in those files
and the reports whose call chain reaches one of them: after a change to `app/users.py`, the
`EFF101` in `app/api.py` that reaches the change through two helpers is shown, and old findings in
files the commit left alone are not. A project can run the hook from its first commit, without a
baseline. A change can also make a report appear in a file the commit left alone without a call chain
through the change, for example when a type annotation that another module relied on is removed;
such a report is not shown here, so run the whole project in continuous integration. A local hook
may also pass the files as paths (`entry: heki check`): a file given to `heki check` is checked in
its project, as with `--touched`. With `--fix`, the hook rewrites only the staged files: the fix
of a shown report in another file is left, and the run says how many were.

```yaml
# .pre-commit-config.yaml
repos:
  - repo: https://github.com/koxudaxi/heki-pre-commit
    rev: v0.5.0
    hooks:
      - id: heki
```

[heki-pre-commit](https://github.com/koxudaxi/heki-pre-commit) installs heki from its PyPI wheel, so
there is no compile and no Rust toolchain. Its tags follow heki releases, so `pre-commit autoupdate`
moves `rev` to the latest release.

To build heki from source instead, point `repo` at `https://github.com/koxudaxi/heki` with a release
`rev`. pre-commit then installs the repository as a Python package, which maturin compiles with cargo
(Rust 1.99 or newer; when cargo is not on `PATH`, the build installs Rust into a cache directory
first). That takes a few minutes, and pre-commit keeps the installed hook for later runs.

A local hook can run any version of the PyPI wheel, also without a compile or a Rust toolchain:

=== "uv"

    ```yaml
    # .pre-commit-config.yaml
    repos:
      - repo: local
        hooks:
          - id: heki
            name: heki
            entry: uvx heki check --touched
            language: system   # needs uv on PATH
            types: [python]
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
            additional_dependencies: [heki]
            types: [python]
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
report of its key, so a run with `--touched` compares all of them, and judges the entries of the
touched files and of the reports it shows: a grown entry fails the run when one of its reports
touches a staged file, and a stale entry elsewhere is left to a run over the whole project.

## Continuous integration

A run fails (exit code 1) on an exact report of an `error` rule: a bug that can be critical, such
as a call that blocks the event loop (`EFF101`). Warnings (slower code, or a bug that is rarely
harmful) and heuristic reports, which can be wrong about the code, are reported without failing
it, whatever their rule's severity: a contract report, heuristic today, is one of them.
`fail-on = "warning"` in `[tool.heki.lint]` (or `--fail-on warning`) makes them fail it too. A
code base with errors today records them in a [Ratchet baseline](#ratchet-baselines) first, so
that the job fails only on new ones; [`heki recommend`](recommend.md) says how many reports fail
the run now.

=== "GitHub Actions annotations"

    ```yaml
    - run: uvx heki check --output-format github
    ```

=== "Code scanning (SARIF)"

    ```yaml
    # The job needs `permissions: security-events: write`.
    - run: uvx heki check --output-format sarif > heki.sarif || test "$?" -eq 1
    - uses: github/codeql-action/upload-sarif@v4
      with:
        sarif_file: heki.sarif
    ```

Warnings and heuristic reports become annotations without failing the step (a heuristic report
of an `error` rule is an `error` annotation); `fail-on = "warning"` in `[tool.heki.lint]` (or
`--fail-on warning`) makes them fail it, as in pre-commit.

In GitHub Actions (`GITHUB_ACTIONS=true`), `heki check` writes these annotations without
`--output-format github`. To keep the text output there, pass `--output-format full`, or set
`HEKI_OUTPUT_FORMAT` (any format) in the job's `env`: it applies whenever the option is not given.

On a pull request, a project with old findings can report only what the change touches, as the
pre-commit hook does (the checkout needs `fetch-depth: 0` for the base branch):

```yaml
- run: git diff -z --name-only --diff-filter=d "origin/$BASE...HEAD" -- '*.py' | xargs -0 -r uvx heki check --touched
  env:
    BASE: ${{ github.base_ref }}
```

`--diff-filter=d` leaves out deleted files, and `xargs -r` skips the run when no Python file
changed.

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
    | [`EFF105`](rules/async-blocking-fs.md), [`EFF107`](rules/unretained-async-task.md) | heuristic | async services |
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
