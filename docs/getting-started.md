# Getting Started

This page runs heki on a project, reads a diagnostic, applies a fix, and picks the rules.

## Run it

From the root of a project (the directory with `pyproject.toml`):

```console
$ heki check
```

heki checks every Python file under the current directory, except the
[excluded and ignored ones](configuration.md#which-files-are-checked). Pass files or directories
to narrow it:

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

| Part | Meaning |
| ---- | ------- |
| `app/api.py:5:12` | where to look |
| `EFF101 (async-blocking-call)` | the rule's code and name |
| `[error]` | its severity |
| the message | what heki proved: here, a chain of calls from an `async def` to a blocking network call |
| `related` lines | the hops of that chain, to follow it across files; editors show them as related information you can jump to |

`heki rule EFF101` explains the rule, its exclusions, and what to do instead. The
[rules reference](rules/index.md) has the same text for every rule.

## Fix what heki can prove

`--fix` applies the rewrites heki proves safe and always faster (`OPT101`, `OPT102`), and
`--diff` shows them first:

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

`--fix` then reports what is left, such as the `EFF101` above, which has no automatic fix.

When heki cannot prove a fix safe, the diagnostic says why and nothing is rewritten. Here the
list is also handed to other code inside the loop:

```console
$ heki check app/shared.py --output-format concise
app/shared.py:4:9: OPT101 `rows = rows + [...]` copies the whole list on every iteration (quadratic); append in place with `rows.append(...)` / `rows.extend(...)` [not auto-fixable: Unique(rows): it may be shared: passed to code heki cannot see (line 5)]
Found 1 diagnostic(s) in 1 file(s).
```

[Fixes and Safety](fixes.md) lists which rules are fixed, and what a fix must prove.

## Silence a diagnostic

Append a suppression comment to the reported line:

```python
user = load_user(user_id)  # heki: ignore[EFF101]
```

The other forms (`# heki: ignore`, `# noqa: EFF101`) and `per-file-ignores` are in
[Inline suppressions](configuration.md#inline-suppressions).

## Choose the rules

```console
$ heki check --select EFF,MUT        # only these families
$ heki check --extend-select EFF105  # add an opt-in rule
$ heki check --ignore OPT110         # everything except one rule
```

Codes, code prefixes, and rule names (`async-blocking-call`) are accepted. A prefix does not
enable the opt-in rules: see [Rule selection](configuration.md#rule-selection-toolhekilint).

## Output formats

```console
$ heki check --output-format json
```

| Format | Prints |
| ------ | ------ |
| `full` (default) | each diagnostic with its source line and call chain |
| `concise` | one line per diagnostic |
| `json` | every field: also the evidence, the blocker that stops a fix, and the fix edits ([fields](coding-agents.md#use-the-cli-in-the-agents-loop)) |
| `github` | GitHub Actions annotations ([CI](adoption.md#continuous-integration)) |
| `sarif` | the message, the location, and the call chain, for code-scanning dashboards (no evidence or blocker) |

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0` | no diagnostics |
| `1` | diagnostics found (or Ratchet regressions, or `--diff` would change files) |
| `2` | configuration error, unreadable file, unknown rule, or syntax error |
