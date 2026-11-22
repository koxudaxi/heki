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
- **Rules worth enabling here**: every rule the configuration leaves off, and every exact rule
  whose heuristic reports it withholds, with the reports naming it in `extend-select` would add
  (exact and heuristic, and with a baseline the regressions among them), its consequence, and
  the published measurement of how often its reports are right about the code
  ([Rule Precision](precision.md)), or "not measured". Rules without reports here are named on
  one line.
- **Contract candidates**: the `write-owners`, `read-owners`, and `effect-boundaries`
  contracts the code already keeps, each with its confidence and evidence, and the trial run of
  its rules that verified it; then the candidates a trial rejected, with the report it made.
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

## Which contracts the block declares

Candidates are drawn from what the code does, never from names:

- `write-owners`: an attribute of a project class written after its object is constructed only
  by functions of one module, and read in at least two other modules. The entry allows the
  smallest of `module:Class.*` and `module:*` that covers the writers. An attribute written
  only while its objects are constructed (by an initializer, or by a function filling the object
  it constructs) gets an entry that allows no writer.
- `read-owners`: a private attribute (`_name`) read only by functions of its class's module,
  whose class is used in at least two modules. The entry allows the readers as above.
- `effect-boundaries`: a module whose functions reach no blocking call, with
  `forbid = ["blocking"]`; a package whose every module qualifies is one scope. A scope has at
  least three functions. Test code, as `relax-async-in-tests` finds it, gets no boundary.

What the configuration already declares is not proposed: an attribute a `write-owners` or
`read-owners` target names, and a module an effect boundary forbidding every blocking effect
covers. A kind whose rule the configuration ignores is not proposed either. Each target has the
module-part form (`app.models:Order.items`), which never matches another project's names.

The contract trial declares every candidate at once, the ownership contracts with
`unresolved = "report"`, and runs their rules (`MUT201` and `MUT205`, `MUT203` and `MUT206`,
`EFF201`) as `heki check` would: inline suppressions and `per-file-ignores` apply, and the
reports they drop are counted apart. A candidate its rule reports is rejected and listed with the
first report. Of each kind, the trial declares the first 32 candidates: `write-owners` with the
attributes changed in place first, then by the modules reading them; `read-owners` by the
modules using the class; boundaries by their functions times the functions calling into them
from outside. The configuration's own `write-owners` contracts count against the 32.

| Confidence | When |
| ---------- | ---- |
| high | the trial reports nothing for the contract, every Python file below the project root was analyzed, and no function of a boundary's scopes reaches code heki does not see (a call it does not resolve, a library name without a catalog entry, a function it does not model) |
| medium | the trial reports no violation, but `MUT205` or `MUT206` reports accesses on objects of unknown class (they are listed, and the entry sets `unresolved = "ignore"`), files were left out, or a scope function reaches code heki does not see (it is listed) |

Medium candidates are in the block too. A candidate on a subclass is left out when the one on
its base class is kept: a target covers the subclasses of its class. Every candidate carries the
caveat that accesses inside code heki cannot follow are not seen.

The verification run then applies the whole block, the rules it enables and the contracts it
declares, as `heki check` would; the counts of the block's rules come from it. A contract that
reports there is dropped, and the run repeats once.

## How the advice is kept honest

- Every count comes from the project, through the analysis `heki check` runs: inline
  suppressions, `per-file-ignores`, and the baseline apply as they do there. With the block
  applied, `heki check` prints the reports it promises.
- A contract enters the block only when a trial of its rules reports nothing on the code as it
  is, and a run with the whole block confirms it.
- How often a rule's reports are right is quoted only from published measurements, with their
  sample, interval, and date; a rule without one is "not measured". No score is computed.
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


def average(rows: list[int]) -> float:
    return sum(rows) / len(rows) if rows else 0.0


def spread(rows: list[int]) -> int:
    return max(rows) - min(rows) if rows else 0
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
heki recommend (experimental) on .: 3 file(s), 11 function(s)

Reports now  1 shown (1 exact, 0 heuristic), 1 heuristic withheld, 5 heuristic rule(s) off

Rules worth enabling here
  rule    reports  exact  consequence  right about the code (published)  real harm  block
  MUT106        1      -  bug          not measured                      -          yes
  OPT110        1      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         no: slower code, not a bug
  No reports here: EFF102, EFF105, EFF106, EFF107, EFF109, EFF110, EFF111, EFF113, MUT101, MUT107, MUT108, MUT111, MUT112, MUT113, MUT115, MUT118, OPT112
  MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, then select it.

Contract candidates
  effect-boundaries  app.report  medium
    3 function(s) in 1 module(s) reach no blocking call heki sees
    trial: EFF201 0
  For every candidate, accesses inside code heki cannot follow (unresolved calls, libraries without a catalog) are not seen.

Notes
  - 1 Python file(s) below the project root were not analyzed (tests/test_users.py): heki takes the analyzed files as the whole project, so what it says about callers, readers, and writers holds for those files only.

pyproject.toml: keys to add or change
  [tool.heki.lint]
  extend-select = ["MUT106"]  # adds MUT106

  [[tool.heki.effect-boundaries]]
  name = "app.report without blocking calls"
  scopes = ["app.report:*"]
  forbid = ["blocking"]
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
| `runs` | the analyses the report was computed from: `discovery`, then `contract-trial` when there are candidates, and `verification` (with whether it `repeated`) when the block adds anything |
| `reports_now` | the counts of "Reports now" |
| `rules` | each rule of "Rules worth enabling here": its metadata, the reports it would add, its published measurement, whether the block enables it, and why |
| `contracts` | each contract candidate the block declares: its kind, what it is about, its entry, confidence, evidence, what the trial reported, and caveats |
| `rejected_contracts` | the candidates a run reported for, with the run and its reports |
| `contracts_not_tried` | per kind, the candidates past the first 32, which no trial declared |
| `notes` | the notes |
| `config`, `config_toml` | the keys to add or change, as keys and values and as TOML |
| `content` | the report as the text output prints it |

Exit codes: `0` the report was printed; `2` configuration, I/O, or syntax errors (the report
of the other files is still printed, as `heki check` does). It never exits with `1`: it checks
nothing.
