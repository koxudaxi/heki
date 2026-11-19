# Adopting heki in a Code Base

## Ratchet baselines

An existing code base usually has findings you cannot fix at once. A Ratchet baseline records
them, fails CI only for **new** ones, and lets the recorded set only shrink:

```console
$ heki check --baseline heki-baseline.toml --update-baseline   # record today's findings
$ heki check --baseline heki-baseline.toml                     # fail on regressions only
```

Entries are keyed by file, function, rule, and target (not by line number), so unrelated
edits and reformatting do not break the baseline. When findings are fixed, `heki check` prints a
note that the entry can shrink; run `--update-baseline` again to lock in the progress. Set
`baseline = "heki-baseline.toml"` in the configuration to use it without the flag.

## Effect snapshots (`MUT103`)

For libraries, a regression in what a public function *does* — it starts mutating an argument or
doing network I/O — is worth reviewing even when no rule fires. Save the effect summaries of all
public functions and check against them:

```console
$ heki snapshot --write                        # writes .heki-summary-snapshot.json
$ heki check --extend-select MUT103            # reports functions that gained effects
```

Commit `.heki-summary-snapshot.json`. When a new effect is intended, refresh the snapshot in the
same pull request; the diff of the JSON file shows reviewers what changed.

## Continuous integration

GitHub Actions annotations:

```yaml
- run: uvx heki check --output-format github
```

Code scanning (SARIF):

```yaml
- run: uvx heki check --output-format sarif > heki.sarif || true
- uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: heki.sarif
```

`--output-format json` gives every field (evidence, blocker, related call chain, fix edits) for
custom tooling.

## Rolling out rules

- Start with the default rules and a baseline, or `--select` one family at a time.
- Declare ownership contracts (`write-owners`) for the shared state you care about; `MUT201`
  reports nothing until a contract exists.
- Keep the opt-in rules (`EFF105`, `MUT103`) for code where they matter: async services and
  library APIs.
- Use `per-file-ignores` rather than broad `ignore` lists, so new code stays covered.
