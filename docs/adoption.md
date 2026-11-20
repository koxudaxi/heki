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

## pre-commit

heki ships as one binary on PyPI, so a hook needs no Rust toolchain. Its checks follow calls
across files, so the hook checks the whole project instead of the staged files only (about 1.5
seconds for 200,000 lines):

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

With a Ratchet baseline configured (`baseline = "heki-baseline.toml"`), the hook fails only on new
findings. Without uv, use `language: python` with `additional_dependencies: [heki]` and
`entry: heki check` instead.

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
- Declare ownership contracts (`write-owners`, `read-owners`) for the shared state you care
  about and effect boundaries (`effect-boundaries`) for the code that must stay pure; `MUT201`,
  `MUT203`, and `EFF201` report nothing until a contract exists.
- Keep the opt-in rules (`EFF105`, `EFF106`, `EFF107`, `MUT103`, `MUT106`) for code where they
  matter: async services, threaded code, and library APIs.
- Use `per-file-ignores` rather than broad `ignore` lists, so new code stays covered.
