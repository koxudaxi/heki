---
name: heki
description: >
  Use this skill when you edit Python code in a project that uses heki (a `[tool.heki]` table in
  pyproject.toml, a heki.toml, or heki among its development dependencies or pre-commit hooks),
  or when the user asks to check Python code for side effects, changes to shared state, blocking
  calls in async code, database queries in loops, or quadratic list, dict, and set patterns.
  heki is a static checker that follows calls across files. The skill runs `heki inspect` before
  editing a function and `heki check --output-format json` after editing, follows `related` to
  the root cause, reads `heki rule <CODE>`, treats heuristic reports as possibly wrong, applies
  only the fixes heki proves safe, and uses `heki check --generate-prompt` when unsure.
license: MIT
metadata:
  status: experimental
---

# heki

heki is a static checker for Python that follows calls across files and reports side effects
and mutations: a blocking call several helpers below an `async def`, a query on every iteration
of a loop, a write to state its owner should make, a quadratic pattern. Every diagnostic says
what was proven, where the evidence is, and what blocks a fix. `heki inspect` says what one
function does, so you do not have to read its callees to find out.

This skill follows [Coding Agents](https://heki.koxudaxi.dev/coding-agents/), the page of
heki's documentation for agents. heki's documentation for models is at
<https://heki.koxudaxi.dev/llms.txt>.

## Availability check

```bash
heki --version
```

If the project lists heki as a development dependency, run it the project's way
(`uv run heki --version`). Otherwise run it once without installing it:

```bash
uvx heki --version
```

Do not add heki to the project, or upgrade it, without the user's approval.

## Workflow

1. Before editing a function, run `heki inspect` on it.
2. Edit.
3. After editing, run `heki check --output-format json` on the project.
4. For each diagnostic, follow `related` to the root cause, read `heki rule <CODE>`, and fix it
   there.
5. Treat a diagnostic with `certainty: "heuristic"` as possibly wrong.
6. Apply only the fixes heki proves safe.
7. When unsure, run `heki check --generate-prompt "<question>"`.

### 1. Before an edit: `heki inspect` (experimental)

```bash
heki inspect app.users.load_user --output-format json
heki inspect app/users.py:42 --output-format json
heki inspect app/users.py::load_user --callers --depth 3 --output-format json
```

- Keep the JSON object. It lists what the function reaches (`effects`), what it changes
  (`changes`), the parameters it keeps (`retains`), what it returns (`returns`), whether it is
  pure (`purity`), its callers, the reports in it, and the contracts that name it.
- After the edit, inspect it again and compare `effects`, `changes`, `retains`, and `returns`: a
  new entry is new behavior. Tell the user about it unless they asked for it.
- An ambiguous or unknown name exits with 2 and lists the candidates (under `error.candidates`
  in JSON): pick one and run the command again.

### 2. After an edit: `heki check`

```bash
heki check --output-format json
```

- Run it on the project (the default `.`), not on the edited file: heki follows calls across
  files, and `heki check app/api.py` misses what its callees do. Filter the JSON by `path`.
- The project's configuration decides which rules run. `--heuristic` also runs the heuristic
  rules that are off by default; use it when the user asks for a broader review.
- Exit codes: 0 nothing fails the run, 1 a diagnostic fails it, 2 a configuration, I/O, or
  syntax error. A file that does not parse is reported on stderr, and the other files are still
  checked.
- The fields of a diagnostic are listed in [references/output.md](references/output.md).

### 3. Fix at the root cause

- Follow `related`: every hop of the call chain behind the diagnostic. The report sits where the
  problem shows (the call in the `async def`, the loop); the cause is usually at the end of the
  chain (the blocking call, the write). Change the code where it happens, or move the call, as
  the rule's documentation says.
- Read `heki rule <CODE>`: what the rule checks, what a report states (the premises of its
  claim), why it matters, and what to do instead. `heki rule <CODE> --output-format json` gives
  the same documentation with the rule's metadata.
- Read `help` for what to do, `evidence` for what was established, and `blocker` for what keeps
  an automatic fix from being safe.
- Never silence a diagnostic with `# heki: ignore[...]` or `# noqa`, and never add it to
  `ignore`, `per-file-ignores`, or a baseline, unless the rule's documentation describes the case
  as intended or the user asks for it.

### 4. Heuristic reports can be wrong

A diagnostic whose `certainty` is `heuristic` may rest on a premise heki could not establish
(the "What a report states" section of `heki rule <CODE>` lists the premises). Before changing
code for it, confirm from the code that what its message says happens. If it does not, leave
the code as it is and tell the user why.

An `exact` diagnostic is proven under heki's assumptions. One that is wrong about the code is a
bug in heki: tell the user, who can report it at <https://github.com/koxudaxi/heki/issues>.

### 5. Apply only proven fixes

```bash
heki check --diff
heki check --fix
```

- `--diff` previews the fixes; `--fix` applies the rewrites heki proves safe and always faster
  (`OPT101`, `OPT102`). A diagnostic's `fix` holds the edits of such a rewrite: apply it with
  `--fix`, not by hand.
- `--include-size-dependent` also applies the proven rewrites that pay off only for large
  inputs. Ask the user before using it.
- Do not rewrite by hand what a `blocker` stops: fix the cause the blocker names, or leave the
  code as it is.

### 6. When unsure: `heki check --generate-prompt` (experimental)

When a diagnostic, its fix, or the contract to write is unclear, print a prompt that holds the
configuration, the rules that apply and what their reports state, the diagnostics of the run,
and how to answer, and work from it (or give it to another model):

```bash
heki check --generate-prompt "Why is load_user reported, and where does the fix go?"
heki check --generate-prompt --output-format json
```

A word after the option is the question, so put paths before it:
`heki check app/ --generate-prompt "..."`. The command writes no file and exits with 0.

## Contracts

When the user wants heki to enforce a rule of the code base that no type states (who may write,
read, or call something, or what a scope must not reach), write a contract under `[tool.heki]`
in `pyproject.toml`: `write-owners`, `read-owners`, `call-owners`, `effect-boundaries`,
`readonly-returns`, or `invalidation`. See
[Contracts](https://heki.koxudaxi.dev/configuration/#contracts). For a draft built from the
project's configuration, run `heki check --generate-prompt "Write a contract that ..."`.

## References

- [references/output.md](references/output.md): the fields of `heki check --output-format json`
  and `heki inspect --output-format json`.
- [references/troubleshooting.md](references/troubleshooting.md): what to do when a command does
  not behave as expected.
