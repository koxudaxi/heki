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

`0`: nothing fails the run. `1`: a diagnostic fails it (an `error`, or a `warning` with
`--fail-on warning`); the output lists it. `2`: a configuration, I/O, or syntax error (below).

## A diagnostic disappears when one file is checked

`heki check path/to/file.py` analyzes that file alone and misses what its callees in other files
do. Check the project (`heki check`, the default `.`), and pass the edited files to `--touched`:
a diagnostic sits where the problem shows, which is often a caller's file, and `--touched` keeps
the ones whose `path` or a `related[].path` is an edited file.

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
