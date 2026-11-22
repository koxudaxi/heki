# Configuration Advice

!!! warning "Experimental"

    `heki recommend` is experimental: what it prints, its policies, and its JSON object can
    still change in any release. See [Experimental Features](experimental.md).

`heki recommend` analyzes a project as `heki check` does and prints advice on its
configuration, with counts from the project and the keys to add to `[tool.heki]`. It writes no
file and runs none of the project's code.

```console
$ heki recommend
$ heki recommend app --output-format json
```

It takes the options of `heki check` that decide what is analyzed: `--select`,
`--extend-select`, `--ignore`, `--heuristic` / `--no-heuristic`, `--config`,
`--target-version`, and `--baseline`. The advice is relative to that configuration.

## What it prints

- **The project**: the root, and the files and functions analyzed.
- **Reports now**: what `heki check` prints with the configuration, as exact and heuristic
  reports (reports sharing a root cause count once, as `heki check` folds them), the heuristic
  reports of exact rules that `lint.heuristic = false` withholds, and the heuristic rules it
  leaves off. With a baseline, the reports it allows and the regressions.
- **What keeps reports heuristic**: the heuristic reports of exact claims (the heuristic reports
  of exact rules, and those of `EFF111` and `EFF113`, which report the claims of `EFF101` and
  `EFF103` with a premise open), counted by the premise of the claim each leaves open and why
  ([Rule Precision](precision.md)), or by the condition of the exact tier that is no premise
  ("Trip count open": the claim holds, and the change pays off only over a loop the data
  sizes). Each row says what the cause states, how many reports leave it open, how many leave
  nothing else open, and what closes it: an annotation, which must be true; a change of the
  code; heki's catalog, which no configuration sets (the row says where to report the case);
  or nothing you can apply. None of it enters the block. A report whose rule records no cause
  is counted as "not recorded", never given a guessed one.
- **Rules worth enabling here**: every rule the configuration leaves off, and every exact rule
  whose heuristic reports it withholds, with the reports naming it in `extend-select` would add
  (exact and heuristic, and with a baseline the regressions among them), its consequence, and
  the published measurement of how often its reports are right about the code
  ([Rule Precision](precision.md)), or "not measured". Rules without reports here are named on
  one line.
- **Notes**: what to know to weigh the advice. When the paths given leave out Python files
  below the project root, the note names them: heki takes the files it analyzed as the whole
  project, so what it says about callers, readers, and writers holds for those files only. It
  counts up to 50 such files, among the first 10,000 entries below the root.
- **The keys to add or change**, for the file the project reads its configuration from:
  `[tool.heki...]` tables in a `pyproject.toml`, tables at the top level in `heki.toml`,
  `.heki.toml`, or a `--config` file. Without a configuration file, the keys go into a new
  `pyproject.toml`. The block only adds: an array the file already sets is written whole, with
  the additions named in a comment, since TOML cannot append to an array.

## Which rules the block enables

A rule enters the block when it reports here, is not experimental, reports bugs (its severity
is `error`), and no published measurement shows real harm below 10%. The other rules are
listed with the reason they stay out:

- slower code, and bugs that are rarely harmful, are worth reading but not worth failing a run
  for;
- experimental rules are listed apart, under "Optional", and never enabled;
- a rule the configuration ignores stays ignored;
- `MUT103` compares with a summary snapshot you save with `heki snapshot --write`;
- contract rules run when their contract is declared, so they are not listed here.

## How the advice is kept honest

- Every count comes from the project, through the analysis `heki check` runs: inline
  suppressions, `per-file-ignores`, and the baseline apply as they do there. With the block
  applied, `heki check` prints the reports it promises.
- How often a rule's reports are right is quoted only from published measurements, with their
  sample, interval, and date; a rule without one is "not measured". No score is computed.
- What keeps a report heuristic comes from the report itself, as the analysis decided its
  tier; a cause the analysis does not record is shown as not recorded, never guessed.
- The same input, configuration, and heki version give the same report, on any machine and
  with any number of threads.
- It writes nothing and keeps every value the configuration sets.

[ADR-020](https://github.com/koxudaxi/heki/blob/main/docs/DESIGN_HISTORY_AND_ADR.md#adr-020-heki-recommend-advice-on-the-configuration-from-the-projects-own-analysis)
records these rules and the decisions behind the command.

## Example

A project with three modules and a test file:

```python
# app/users.py
import requests

def fetch_name(user_id: int) -> str:
    return requests.get(f"https://api.example.com/users/{user_id}", timeout=5).text

def load_user(user_id: int) -> dict[str, str]:
    return {"id": str(user_id), "name": fetch_name(user_id)}

def known_names(names: list[str], source: list[str]) -> list[str]:
    known = list(source)
    found = []
    for name in names:
        if name in known:
            found.append(name)
    return found
```

```python
# app/report.py
def total_and_count(rows: list[int]) -> tuple[int, int]:
    items = (r for r in rows)
    total = sum(items)
    count = len(list(items))
    return total, count
```

```python
# app/api.py
from app.users import load_user

async def get_user(user_id: int) -> dict[str, str]:
    return load_user(user_id)
```

```python
# tests/test_users.py
def test_load_user() -> None:
    pass
```

<!-- Generated by scripts/build_docs.py from heki recommend on the example above: report. -->

```console
$ heki recommend app
heki recommend (experimental) on .: 3 file(s), 9 function(s)

Reports now  1 shown (1 exact, 0 heuristic), 1 heuristic withheld, 5 heuristic rule(s) off

What keeps reports heuristic
  1 heuristic report(s) of exact claims: OPT110 1
  premise: cause                                 reports  only open  closed by
  Rewrite proven safe: zero-trip-exception-safe        1          0  code
    the `set(...)` the rewrite builds before the loop may raise even when the loop runs no time: an element that may be unhashable, or a name that may be unbound there
    e.g. app/users.py:14 OPT110
  Rewrite proven safe: hash-consistent-probe           1          0  code
    the value looked up may compare otherwise through `hash` than through `==`: a custom `__eq__` / `__hash__`, or an unhashable value
    e.g. app/users.py:14 OPT110
  Nothing here enters the block: an annotation or a change of the code is an edit of the program.

Rules worth enabling here
  rule    reports  exact  consequence  right about the code (published)  real harm  block
  MUT106        1      -  bug          not measured                      -          yes
  OPT110        1      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         no: slower code, not a bug
  No reports here: EFF102, EFF105, EFF106, EFF107, EFF109, EFF110, EFF111, EFF113, MUT101, MUT107, MUT108, MUT111, MUT112, MUT113, MUT115, MUT118, OPT112
  MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, then select it.

Notes
  - 1 Python file(s) below the project root were not analyzed (tests/test_users.py): heki takes the analyzed files as the whole project, so what it says about callers, readers, and writers holds for those files only.

pyproject.toml: keys to add or change
  [tool.heki.lint]
  extend-select = ["MUT106"]  # adds MUT106
```

<!-- End of the generated report. -->

## JSON output

`--output-format json` prints one object, described by the JSON Schema at
[schemas/recommend-1.json](schemas/recommend-1.json), the URL its `$schema` field names, which
`heki recommend --output-format-json-schema` prints.
Within a `version`, fields are only added; the `version` changes when a field is removed or
changes meaning.

| Field | Content |
| ----- | ------- |
| `kind`, `version`, `heki` | `heki-recommend`, the version of the object, and the version of heki |
| `root`, `config_file`, `table_prefix` | the project root, the configuration file (`null` without one), and the table its keys are under (`tool.heki`, or empty) |
| `analyzed` | the files and functions analyzed, the files that could not be read or parsed, the paths given, and the Python files below the root they leave out |
| `runs` | the analyses the report was computed from |
| `reports_now` | the counts of "Reports now" |
| `open_premises` | the counts of "What keeps reports heuristic": by code; by premise or condition and cause, with what the cause states, what closes it, the reports, the reports it is the only open cause of, and the first 50 reports; the sets of causes that reports leave open together |
| `rules` | each rule of "Rules worth enabling here": its metadata, the reports it would add, its published measurement, whether the block enables it, and why |
| `notes` | the notes |
| `config`, `config_toml` | the keys to add or change, as keys and values and as TOML |
| `content` | the report as the text output prints it |

Exit codes: `0` the report was printed; `2` configuration, I/O, or syntax errors (the report
of the other files is still printed, as `heki check` does). It never exits with `1`: it checks
nothing.
