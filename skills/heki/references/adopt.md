# Adopting heki in a repository

Read this when the user asks you to adopt heki in a repository. Do the steps in order, tell the
user what each one found, and stop where a step says to ask.

## 1. Look before changing anything

1. Read `pyproject.toml`: the Python version, the development dependencies, and an existing
   `[tool.heki]` table or `heki.toml`.
2. Run `heki check` from the project root and read the reports and the closing note. An exact
   error fails the run; warnings and heuristic reports do not.
3. Run `heki recommend` (experimental). It writes no file. It says which rules would report on
   the project, what heki could not see, and the contracts the code already keeps, with the keys
   to add.

## 2. Propose one or two contracts

Pick the first one or two contracts from what `heki recommend` lists as kept by the code today,
and show the user the lines you would add, with the reason. Do not propose the whole list: a
contract the team has not agreed on becomes a report nobody reads. Wait for the user's answer
before writing it.

## 3. Write the files

1. Put the contracts, `fail-on`, and the baseline path in `heki.toml`, not in code the agent edits
   as part of its task. heki reads only the first of `heki.toml`, `.heki.toml` and `[tool.heki]` in
   `pyproject.toml` that exists, without merging them. If the repository already configures heki in
   `.heki.toml` or `pyproject.toml`, add to that file, or move all of its settings into the new
   `heki.toml` in the same change.
2. Add a `CODEOWNERS` line for `heki.toml` (or the host's rule for protected files), so that a
   change to a contract is a reviewed change and a report cannot be removed by moving the wall.
3. When `heki check` reports old findings, record them once:
   `heki check --baseline heki-baseline.toml --update-baseline`. Commit the file and set
   `baseline = "heki-baseline.toml"` in `heki.toml`. The job then fails only on new findings.
4. Add the pre-commit hook and the CI job from
   [Adopting heki](https://heki.koxudaxi.dev/adoption/#pre-commit). The hook passes the staged
   files to `--touched`; CI runs the whole project.

Do not add or upgrade heki as a dependency without the user's approval.

## 4. Run the whole project before finishing

`--touched` is for the loop while editing. Before you say the work is done, run
`heki check` on the whole project (with the baseline, when there is one) and report its exit
code. Then run the project's own tests and linters, and say if any of them fails.
