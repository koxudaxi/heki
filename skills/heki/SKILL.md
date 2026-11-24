---
name: heki
description: >
  Use after editing Python code in a project that uses heki (a `[tool.heki]` table in
  pyproject.toml, a heki.toml, or heki among its development dependencies or pre-commit hooks),
  or when the user asks to check Python code for side effects, changes to shared state, blocking
  calls in async code, queries in loops, or quadratic patterns. After the edit, run
  `heki check --touched <edited files> --output-format json` and act on the reports the edit
  caused.
license: MIT
metadata:
  status: experimental
---

# heki

heki is a static checker for Python that follows calls across files. This skill is the routine of
[Coding Agents](https://heki.koxudaxi.dev/coding-agents/) after an edit.

Run `heki`, or `uv run heki` when the project lists it as a development dependency, or
`uvx heki`. Do not add or upgrade heki without the user's approval.

## After an edit

1. Run `heki check --touched <edited files> --output-format json` in the project: heki analyzes
   the project and shows the reports whose `path` or a `related[].path` is an edited file, as a
   report sits where the problem shows, often in a caller's file.
2. Act on the reports it shows. On a project with old findings, record them once before editing
   with `heki check --baseline <file outside the project> --update-baseline` and add
   `--baseline <file>` to each check: it then reports only new findings.
3. Follow `related` to the root cause, read `heki rule <CODE>`, and fix it there as `help` says.
4. A `heuristic` report can be wrong: confirm from the code what its message says before
   changing anything, and tell the user when it does not hold. A wrong `exact` report is a bug
   in heki: tell the user.
5. Never suppress a report (`# heki: ignore[...]`, `# noqa`, `ignore`, a baseline) or weaken a
   contract unless the rule's documentation describes the case as intended or the user asks.
6. Apply rewrites only with `heki check --fix` (`--diff` previews them), and leave what a
   `blocker` stops.

## On demand

- `heki inspect <function> --output-format json` (experimental): what a function reaches,
  changes, keeps, and returns, when a report or a question is about it.
- `heki check --generate-prompt "<question>"` (experimental): a prompt with the configuration,
  the rules, and the reports, when a report or its fix is unclear.
- [Contracts](https://heki.koxudaxi.dev/configuration/#contracts), when the user wants heki to
  enforce who may write, read, or call something, or what a scope must not reach.

[references/output.md](references/output.md) lists the JSON fields, and
[references/troubleshooting.md](references/troubleshooting.md) says what to do when a command
fails or a report seems wrong.
