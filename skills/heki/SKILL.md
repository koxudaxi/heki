---
name: heki
description: >
  Use after editing Python code in a project that uses heki (a `[tool.heki]` table in
  pyproject.toml, a heki.toml, or heki among its development dependencies or pre-commit hooks),
  or when the user asks to check Python code for side effects, changes to shared state, blocking
  calls in async code, queries in loops, or quadratic patterns. After the edit, run
  `heki check --touched <edited files> --output-format json` and act on the reports the edit
  caused; before finishing, run `heki check` on the whole project.
license: MIT
metadata:
  status: experimental
---

# heki

heki is a static checker for Python that follows calls across files. This is the routine of
[Coding Agents](https://heki.koxudaxi.dev/coding-agents/).

Run `heki`, or `uv run heki` when the project lists it as a development dependency, or
`uvx heki`. Do not add or upgrade heki without the user's approval.

## After an edit

1. Run `heki check --touched <edited files> --output-format json` in the project: it shows the
   reports whose `path` or a `related[].path` is an edited file (often in a caller).
2. Act on the reports it shows. With old findings, first record them with
   `heki check --baseline <file outside the project> --update-baseline`, and add
   `--baseline <file>` to each check.
3. Follow `related` to the root cause, read `heki rule <CODE>`, and fix it there as `help` says.
4. A `heuristic` report can be wrong: confirm from the code what its message says before
   changing anything, and tell the user when it does not hold; a wrong `exact` report is a bug
   in heki.
5. Never suppress a report (`# heki: ignore[...]`, `# noqa`, `ignore`, a baseline) or weaken a
   contract unless the rule's documentation describes the case as intended or the user asks.
6. Apply rewrites only with `heki check --fix` (`--diff` previews them), and leave what a
   `blocker` stops.
7. Before you finish, run `heki check --output-format json` without `--touched` (with step 2's
   `--baseline`), act on it, and tell the user its exit code: an edit can add a report that
   touches no edited file.

## On demand

- `heki inspect <function> --output-format json` (experimental): what a function reaches,
  changes, keeps, and returns, when a report or question is about it.
- `heki check --generate-prompt "<question>"` (experimental): a prompt with the configuration,
  rules, and reports, when a report or its fix is unclear.
- [references/contracts.md](references/contracts.md), before you draft or change a contract: thin
  walls, old violations in a baseline, never in `allowed-*`.

[references/output.md](references/output.md) lists the JSON fields, and
[references/troubleshooting.md](references/troubleshooting.md) says what to do when a command
fails or a report seems wrong.

To adopt heki in a repository, follow [references/adopt.md](references/adopt.md).
