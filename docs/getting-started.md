# Getting Started

This page runs heki on a project, reads a diagnostic, applies a fix, and picks the rules.

## Run it

From the root of a project (the directory with `pyproject.toml`):

```console
$ heki check
$ heki check --heuristic
```

The first command runs the exact rules (hand-checked samples of their reports found none wrong
about the code) and the rules of the [contracts](configuration.md#contracts) you declare. No rule
is exact yet, so without a contract it reports nothing for now, and says how many heuristic rules
it left off. `--heuristic` runs those too; their reports can be wrong about the code, and every
output labels them ([Rule Precision](precision.md)).

heki checks every Python file under the current directory, except the
[excluded and ignored ones](configuration.md#which-files-are-checked). Pass files or directories
to narrow it:

```console
$ heki check src/ tests/test_api.py
```

## Read a diagnostic

```console
app/api.py:5:12: EFF101 (async-blocking-call) [warning, heuristic] `handler` reaches the blocking call `requests.get` on the event loop
  |
5 |     user = load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `handler` calls `app.users.load_user`
  = related: app/users.py:5:12: `load_user` calls `requests.get`
  = help: offload it with `asyncio.to_thread`, or use an async client
```

| Part | Meaning |
| ---- | ------- |
| `app/api.py:5:12` | where to look |
| `EFF101 (async-blocking-call)` | the rule's code and name |
| `[warning, heuristic]` | its severity: `error` for a bug (a wrong result, an exception, a lost update, a deadlock, a broken contract), `warning` for slower code, as here. An error makes `heki check` fail; a warning is shown without failing it, unless `--fail-on warning` (`lint.fail-on = "warning"`) is set. `heuristic` marks a report that can be wrong about the code: a premise of its claim can be open |
| the message | what heki proved, in one sentence: here, an `async def` reaches a blocking network call |
| `related` lines | the hops of the chain of calls behind it, to follow it across files; editors show them as related information you can jump to |
| `help` line | what to do about it |

`heki rule EFF101` explains the rule, its exclusions, and what to do instead. The
[rules reference](rules/index.md) has the same text for every rule.

## Fix what heki can prove

`--fix` applies the rewrites heki proves safe and always faster (`OPT101`, `OPT102`), and
`--diff` shows them first. Both rules are heuristic, so they run with `--heuristic`:

```console
$ heki check --heuristic --diff
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

When heki cannot prove a fix safe, the help says why and nothing is rewritten. Here the list is
also handed to other code inside the loop:

```console
$ heki check app/shared.py
app/shared.py:4:9: OPT101 (list-quadratic-concat) [warning] `rows = rows + [...]` copies the whole list on every iteration (quadratic)
  |
4 |         rows = rows + [item]
  |         ^
  = help: append in place with `rows.append(...)` / `rows.extend(...)` [not auto-fixable: Unique(rows): it may be shared: passed to code heki cannot see (line 5)]

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
enable the opt-in rules, nor the heuristic ones without `--heuristic`: see
[Rule selection](configuration.md#rule-selection-toolhekilint).

## Output formats

```console
$ heki check --output-format json
```

| Format | Prints |
| ------ | ------ |
| `full` (default) | each diagnostic with its source line, call chain, and help |
| `concise` | one line per diagnostic: the message, without the help |
| `json` | every field: also the help, the evidence, the blocker that stops a fix, and the fix edits ([fields](coding-agents.md#use-the-cli-in-the-agents-loop)) |
| `github` | GitHub Actions annotations with the message ([CI](adoption.md#continuous-integration)) |
| `sarif` | the message, the location, the call chain, and the help (`properties.help`), for code-scanning dashboards (no evidence or blocker) |

Every format, `--diff`, and the errors of files that cannot be read or parsed print a file's
path relative to the working directory when the file is inside it (otherwise as given), as Ruff
does, and with `/` separators on every platform, Windows included.

### Colors

On a terminal, the `full` and `concise` output and `--diff` are colored as Ruff colors its
output: paths in bold, rule codes in red, the severity, the source frame and its caret, and the
lines a diff removes and adds. The same variables and option as Ruff's decide it, each row over
the rows above it:

| Setting | Colors |
| ------- | ------ |
| stdout is a terminal / a pipe or a file | on / off |
| `NO_COLOR` set (to anything), or `CLICOLOR=0` | off |
| `FORCE_COLOR` set to anything but the empty string, or `CLICOLOR_FORCE` to anything but `0` | on (for a CI log that shows colors) |
| `--color always` / `--color never` | on / off |

`json`, `github`, and `sarif` output never holds colors, and the summary and the errors heki
writes to stderr are not colored.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0` | no diagnostics, or only warnings (they are printed) |
| `1` | an error found, or a warning with `--fail-on warning` (`lint.fail-on = "warning"`), among the Ratchet regressions when a baseline is used; also when `--update-baseline` refuses a grown entry, or `--diff` would change files |
| `2` | configuration error, unreadable file, unknown rule, or syntax error |
