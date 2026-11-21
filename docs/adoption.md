# Adopting heki in a Code Base

An existing code base usually has findings you cannot fix at once. This page shows how to adopt
heki step by step:

1. Record today's findings in a [Ratchet baseline](#ratchet-baselines), so heki fails only on
   new ones.
2. Run it in [pre-commit](#pre-commit) and [continuous integration](#continuous-integration).
3. [Roll out more rules](#rolling-out-rules): contracts for your shared state, and the opt-in
   rules where they matter.
4. For a library, save [effect snapshots](#effect-snapshots-mut103) of its public functions.

## Ratchet baselines

A Ratchet baseline records the findings you have today and fails only on new ones:

```console
$ heki check --baseline heki-baseline.toml --update-baseline   # record today's findings
$ heki check --baseline heki-baseline.toml                     # fail on regressions only
```

Set `baseline = "heki-baseline.toml"` in the [configuration](configuration.md#top-level-settings)
to use it without the flag, also in `--watch` and the language server.

Entries are keyed by file, function, rule, and target, not by line number, so unrelated edits and
reformatting do not break the baseline:

```toml
# heki Ratchet baseline: counts may only shrink (`heki check --update-baseline`).
version = 2

[[entries]]
path = "app/api.py"
qualname = "get_user"
rule_code = "EFF101"
target = "requests.get"
count = 1
```

The recorded set can only shrink:

| Situation | `heki check` | `heki check --update-baseline` |
| --------- | ------------ | ------------------------------ |
| No baseline file yet | an error (exit code 2) | records today's findings |
| A finding was fixed | a note that the entry can shrink | records the smaller entry |
| An entry grew, or a new finding appeared | reports it, with a note on the allowed and found counts (exit code 1) | reports it the same way and leaves the file as it is (exit code 1) |
| A file could not be analyzed (a syntax error, an unreadable file) | reports the error (exit code 2) | writes nothing, so the entries of that file are not lost (exit code 2) |

To record new findings on purpose, delete the baseline and record it again.

### What an entry counts

Each entry counts violation sites, not reports (`sites` in JSON output):

- a contract report at the caller of a private helper stands for every write, read, or call
  inside the helper;
- an effect boundary report stands for every forbidden call or write its scope function reaches.

So a second violation behind a recorded report is a regression too.

A baseline without `version = 2` was written before heki counted sites, and counts reports:

- `heki check` says so in a note, and a second site behind a recorded report is no regression;
- `--update-baseline` compares in the old unit first, so nothing that grew is taken in, then
  rewrites the file with `version = 2`.

A version this heki does not know is an error, and so is a file listing one entry twice (a hand
merge of two baselines).

## pre-commit

heki ships as one binary on PyPI, so a hook needs no Rust toolchain. Its checks follow calls
across files, so the hook checks the whole project instead of the staged files only.

=== "uv"

    ```yaml
    # .pre-commit-config.yaml
    repos:
      - repo: local
        hooks:
          - id: heki
            name: heki
            entry: uvx heki check
            language: system   # needs uv on PATH
            types: [python]
            pass_filenames: false
    ```

=== "Without uv"

    ```yaml
    # .pre-commit-config.yaml
    repos:
      - repo: local
        hooks:
          - id: heki
            name: heki
            entry: heki check
            language: python
            additional_dependencies: [heki]
            types: [python]
            pass_filenames: false
    ```

With a baseline configured, the hook fails only on new findings.

## Continuous integration

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

With SARIF:

- findings (exit code `1`) become code-scanning alerts and keep the step green;
- a configuration, read, or syntax error (exit code `2`) still fails the step: heki then writes
  SARIF only for the files it could analyze, which would hide the others;
- results carry the message, the location, the call chain (`relatedLocations`), and the help
  (`properties.help`; SARIF's `rules[].help` is one text per rule, and a report's help can name
  the object it is about).

For custom tooling, `--output-format json` gives every field: see
[Coding Agents](coding-agents.md#use-the-cli-in-the-agents-loop).

## Rolling out rules

1. Start with the default rules and a baseline, or `--select` one family at a time. Only the
   exact rules run by default; add the heuristic ones with `lint.heuristic = true` (or
   `--heuristic`), or name the ones you want in `extend-select`. Their reports can be wrong
   about the code, and every output labels them `heuristic`
   ([Rule Precision](precision.md)).
2. Declare [contracts](configuration.md#contracts) for the shared state and the operations you
   care about, and effect boundaries for the code that must stay pure. `MUT201`, `MUT203`,
   `MUT204`, `MUT109`, and `EFF201` report nothing until a contract exists.
3. Enable the opt-in rules where they matter:

    | Rule | Certainty | Turn it on for |
    | ---- | --------- | -------------- |
    | [`EFF105`](rules/async-blocking-fs.md), [`EFF107`](rules/unretained-async-task.md) | heuristic | async services |
    | [`EFF106`](rules/lock-or-tx-held-across-io.md) | heuristic | threaded code, and code that holds database transactions |
    | [`MUT103`](rules/summary-effect-regression.md) | heuristic | library APIs (with [effect snapshots](#effect-snapshots-mut103)) |
    | [`MUT106`](rules/iterator-double-consumption.md) | heuristic | code that passes iterators and generators around |
    | [`MUT108`](rules/readonly-type-mutation.md) | heuristic | code that relies on frozen dataclasses and `ReadOnly` fields |

4. Use `per-file-ignores` rather than broad `ignore` lists, so new code stays covered.

### From heki 0.1

heki 0.1 ran every rule that is not opt-in by default. Since rules have a certainty, only the
exact ones do, and no rule is exact yet:

- `lint.heuristic = true` restores the rules of 0.1, now labelled `heuristic` in every output.
- A baseline keeps its entries: their keys (file, function, rule, target) do not change.
- `heki check --fix` applies the fixes of `OPT101` and `OPT102` only when they run: with
  `lint.heuristic = true`, or when `select` or `extend-select` names them.

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
