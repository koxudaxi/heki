# Troubleshooting

## The command is not found

Run heki once without installing it, or the project's way when it is a development dependency:

```bash
uvx heki --version
uv run heki --version
```

The package and the command are both named `heki`. It is a single executable and needs no Python
interpreter to run.

## A diagnostic disappears when one file is checked

`heki check path/to/file.py` analyzes that file alone and misses what its callees in other files
do. Check the project (`heki check`, the default `.`) and filter the JSON by `path`.

## `heki inspect` exits with 2

The symbol names no single function: it is ambiguous, unknown, a class, or a module. The error
lists the candidates (under `error.candidates` with `--output-format json`); inspect one of them,
by its dotted name, `path.py:LINE`, or `path.py::NAME`.

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

## `MUT103` warns that the snapshot does not exist

`MUT103` compares the functions' effects with a saved snapshot. Write one with
`heki snapshot --write` only when the user wants to record the current effects.
