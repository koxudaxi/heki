# Troubleshooting

## The command is not found

Run heki once without installing it, or the project's way when it is a development dependency:

```bash
uvx heki --version
uv run heki --version
```

The package and the command are both named `heki`. It is a single executable and needs no Python
interpreter to run.

## What the exit code says

`0`: nothing fails the run. `1`: a diagnostic fails it (an exact `error`, or any diagnostic with
`--fail-on warning`: a heuristic one counts as a warning); the output lists it. `2`: a configuration, I/O, or syntax error (below).

## The whole project reports what `--touched` did not show

`--touched` shows the diagnostics in the edited files and those whose call chain passes through
one. An edit can also add a diagnostic elsewhere without such a chain: a module re-exports a name
from another one, or another module relied on an annotation the edit changed. No run over the
edited files can tell, so without a baseline it ends with the note "N report(s) that would fail
the run are not shown" (on stderr, also with `--output-format json`), and with a baseline it shows
every regression wherever it is. Naming `pyproject.toml` or `heki.toml` shows every diagnostic.
Run `heki check` on the whole project before finishing, and fix what it reports.

## A diagnostic disappears when a directory is checked

`heki check path/to/dir` (or `heki check` run in a directory below the project root) analyzes
that directory alone and misses what its callees in other directories do; a note names the
project modules its files import that the run left out. Check from the project root, and pass the
edited files to `--touched` or name them as paths (a file is checked in its project): a diagnostic
sits where the problem shows, which is often a caller's file, and the run keeps the ones whose
`path` or a `related[].path` is an edited file.

## `heki inspect` exits with 2

The symbol names no single function: it is ambiguous, unknown, a class, or a module. The error
says what to do (`error.help` with `--output-format json`; a misspelled name gets the functions
whose names look like it) and lists the functions to choose from (`error.candidates`,
`error.suggestions`, `error.methods`, or `error.functions`); inspect one of them, by its dotted
name, `path.py:LINE`, or `path.py::NAME`. With `-`, that error object stands in the array in the
name's place, after and before the reports of the other names.

## A command exits with 2 before checking anything

The configuration, a path, or an option is wrong. The error names the file and line, says what
to write instead (`= help:`), and links the section of the documentation about it (`= docs:`).
With `--output-format json`, an error in the configuration or a path is `{"error": {...}}` on
stdout: `help`, `docs`, and `candidates` (the keys, rules, or values a misspelled one may have
meant). An unknown or invalid option is reported before that, as text on stderr only. Apply the help: for
example, `select` written in `[tool.heki]` belongs in `[tool.heki.lint]`.

## `heki check` exits with 2 and still prints diagnostics

A file could not be read or did not parse. heki checks the other files and names the failed one
on stderr; nothing the failed file does is known, so fix it first.

## A diagnostic seems wrong

- `certainty: "heuristic"`: a premise of its claim can be open. Confirm what its message says
  from the code; if it does not hold, leave the code and tell the user why.
- `certainty: "exact"`: it is proven under heki's assumptions (`heki rule <CODE>` lists the
  premises). If the code shows that it is wrong, it is a bug in heki: tell the user.
- Do not silence it with `# heki: ignore[...]`, `# noqa`, `ignore`, `per-file-ignores`, or a
  baseline unless the rule's documentation describes the case as intended or the user asks.

## A text run says heuristic rules are off

`note: N heuristic rule(s) are off` means the project runs only the exact rules and the contract
rules. `--heuristic` runs the others too; their reports can be wrong about the code.

## Only some diagnostics appear

With a Ratchet baseline (`--baseline` or `baseline` in the configuration), `heki check` reports
only the findings the baseline does not allow. Do not update the baseline to make a run pass.

## `--fix` leaves a reported rewrite

`--fix` applies the rewrites heki proves safe and always faster. `--include-size-dependent` also
applies the proven ones that pay off only for large inputs: ask the user before using it.

## `MUT103` warns that the snapshot does not exist

`MUT103` compares the functions' effects with a saved snapshot. Write one with
`heki snapshot --write` only when the user wants to record the current effects.
