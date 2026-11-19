# Getting Started

## Run it

From the root of a project (the directory with `pyproject.toml`):

```console
$ heki check
```

With no arguments heki checks every Python file under the current directory, honoring
`.gitignore`, `.hekiignore`, and the `exclude` setting. Pass files or directories to narrow it:

```console
$ heki check src/ tests/test_api.py
```

## Read a diagnostic

```console
app/api.py:5:12: EFF101 (async-blocking-call) [error] `async def handler` calls `app.users.load_user`, which reaches a blocking call `requests.get` (blocking_net) on the event loop; offload it (`asyncio.to_thread`) or use an async API
  |
5 |     user = load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `handler` calls `app.users.load_user`
  = related: app/users.py:5:12: `load_user` calls `requests.get`
```

- `app/api.py:5:12` is where to look; `EFF101` / `async-blocking-call` the rule, with its
  severity.
- The message says what heki proved: here, a chain of calls from an `async def` to a blocking
  network call. The `related` lines are the hops of that chain, so you can follow it across
  files (editors show them as clickable related information).
- `heki rule EFF101` explains the rule, its exclusions, and what to do instead. The
  [rules reference](rules/index.md) has the same text for every rule.

## Fix what heki can prove

Rules in the `OPT` family come with rewrites. The safe, always-profitable ones (`OPT101`,
`OPT102`) are applied with `--fix`; `--diff` shows them first:

```console
$ heki check --diff
--- a/app/report.py
+++ b/app/report.py
@@ -1,5 +1,5 @@
 def rows_of(items):
     rows = []
     for item in items:
-        rows = rows + [item]
+        rows.append(item)
     return rows
Would fix 1 issue(s) in 1 file(s).
$ heki check --fix
Fixed 1 issue(s) in 1 file(s).
```

`--fix` then reports what is left (here the `EFF101` above, which has no automatic fix).

A fix is applied only when heki proved it does not change what the program does. When it cannot,
the diagnostic says why and nothing is rewritten, for example when the list is also handed to
other code inside the loop:

```console
$ heki check app/shared.py --output-format concise
app/shared.py:4:9: OPT101 `rows = rows + [...]` copies the whole list on every iteration (quadratic); append in place with `rows.append(...)` / `rows.extend(...)` [not auto-fixable: Unique(rows): it may be shared: passed to code heki cannot see (line 5)]
Found 1 diagnostic(s) in 1 file(s).
```

See [Fixes and Safety](fixes.md).

## Silence a diagnostic

Append a suppression comment to the reported line:

```python
user = load_user(user_id)  # heki: ignore[EFF101]
```

`# heki: ignore` without codes silences every rule on the line; `# noqa: EFF101` works too.
To keep a rule away from whole files, use `per-file-ignores` in the
[configuration](configuration.md).

## Choose the rules

```console
$ heki check --select EFF,MUT        # only these families
$ heki check --extend-select EFF105  # add an opt-in rule
$ heki check --ignore OPT110         # everything except one rule
```

Codes, code prefixes, and rule names (`async-blocking-call`) are all accepted. Opt-in rules
(`EFF105`, `MUT103`) are enabled only by their exact code or name.

## Machine-readable output

```console
$ heki check --output-format json    # also: full (default), concise, github, sarif
```

`json` includes the evidence behind each diagnostic, the blocker that stops a fix, the call chain,
and the fix edits. `github` prints GitHub Actions annotations; `sarif` feeds code-scanning
dashboards.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0` | no diagnostics |
| `1` | diagnostics found (or Ratchet regressions, or `--diff` would change files) |
| `2` | configuration error, unreadable file, unknown rule, or syntax error |
