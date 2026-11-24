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
- **What heki cannot see**: where heki is silent, so no count of reports applies (see
  [below](#what-heki-cannot-see)).
- **Rules worth enabling here**: every rule the configuration leaves off, and every exact rule
  whose heuristic reports it withholds, with the reports naming it in `extend-select` would add
  (exact and heuristic, and with a baseline the regressions among them), its consequence, and
  the published measurement of how often its reports are right about the code
  ([Rule Precision](precision.md)), or "not measured". Rules without reports here are named on
  one line.
- **Contract candidates**: the `write-owners`, `read-owners`, and `effect-boundaries`
  contracts the code already keeps, each with its confidence and evidence, and the trial run of
  its rules that verified it; then the candidates a trial rejected, with the report it made.
- **Contract entries that match nothing**: the entries of the contracts configured now that name
  no definition, attribute, method, or `__dict__` key in the analyzed files, with the existing
  names like each, as `heki check` warns about them
  ([Entries that match nothing](configuration.md#entries-that-match-nothing)). They are listed
  whatever `lint.unmatched-contract-entries` says, and the section is left out when there is
  none.
- **Notes**: what to know to weigh the advice. When the paths given leave out Python files
  below the project root, the note names them: heki takes the files it analyzed as the whole
  project, so what it says about callers, readers, and writers holds for those files only. It
  counts up to 50 such files, among the first 10,000 entries below the root.
- **The keys to add or change**, for the file the project reads its configuration from:
  `[tool.heki...]` tables in a `pyproject.toml`, tables at the top level in `heki.toml`,
  `.heki.toml`, or a `--config` file. Without a configuration file, the keys go into a new
  `pyproject.toml`. The block only adds: an array the file already sets is written whole, with
  the additions named in a comment, since TOML cannot append to an array.

## What heki cannot see

heki reports nothing through a call it cannot follow, so these parts list calls, not reports.
For each call it says how many are awaited, how many may hold up an event loop (not awaited, in
an `async def` or in a function one runs on its thread), and how many run in a loop or below
one.

- **Calls of packages heki's catalog does not cover**, by the name an entry of
  `[[tool.heki.ecosystem]]` needs (entries are looked up by that exact name, without
  inheritance or aliases). heki takes such a call to have no effect. For the 64 names with the
  most reach, a trial run gives each a catalog entry that waits on the network and sends a
  database query (`effects = ["blocking_net", "db_query"]`) and says what `heki check` would
  then print through it: "if it waits" (`EFF101`, `EFF111`) and "if it sends a query"
  (`EFF103`, `EFF113`). heki cannot know what a library call does, so no entry enters the
  block, and the trial's counts are kept out of every count of reports: add an entry if you know
  what the call does.
- **Names heki's catalog lacks in packages it covers**: the same, for the names the trial shows
  heki would report through. If such a call waits or sends a query, report it at heki's issue
  tracker: the catalog should list it.
- **Calls heki does not resolve**, by function and reason: a parameter without annotation, an
  annotation that names no class heki knows, a value annotated `Any`, `object`, or `Callable`, a
  name nothing heki sees binds, a protocol whose implementations heki cannot list, a call's
  result or an attribute heki gives no class, a method named at run time, more implementations
  than heki follows, or a member heki does not find. An annotation would resolve most of them;
  an annotation is an edit of the program, so none enters the block.
- **Source roots**: a directory below the project root, neither a source root nor a regular
  package (a directory with `__init__.py` is imported by its own name, so heki never takes one
  for a source root), below which the dotted path of an import that names no project module is a
  file (`backend/config/env.py` for `import config.env`) is a candidate for `src`. An import of
  the standard library or of a package heki's catalog covers is no candidate's, and neither is
  one whose path is below several directories none of which holds the importing file. The first
  8 candidates by imports are tried, each added to `src` with those already taken (the source
  roots heki detects are written out with them, since setting `src` turns detection off). A
  trial computes the module names with the directory added for every Python file below the
  project root, those the paths given leave out included (heki reads them for this; when they
  are more than the 50 it reads, no directory is tried), resolves each import as heki's index
  does, and takes the directory into the block only when the imports it was tried for name its
  own files, no two files of one kind then define one module unless just those files did before,
  every import that names a module of the project now, and every import a directory already
  taken was taken for, still names the same module of the same file, and every contract of the
  configuration and every baseline entry still names what it names now (the rules' matchers
  judge each definition by its name now and its name with the directory; a baseline entry must
  not spell a module whose name would change). The block is then checked whole: when it would
  make some other import resolve into a file below a source root that does not hold the
  importing file (`import utils` in `tool.py`, below `svc_a/` and `svc_b/`, would name
  `svc_a/utils.py`), the directory that file is below stays out and the others are tried again.
  heki cannot tell such a file is the one meant, and the block holds only what the trial
  checked. The calls of names of those files are left out of the library names.
- **Functions with an A1 hazard** (frame introspection, `exec`, a body too large to model):
  the Soundness lane proves nothing in them. `heki assumptions` lists them.

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
  least three functions. A package holding test code names its other modules one by one, not by
  a wildcard.

No candidate is drawn from a module holding test code, as `relax-async-in-tests` finds it: a
test file, or a file defining a `test_*` function or method (a test class in Django's
`tests.py`, for one). Its classes get no ownership contract and its functions no boundary. Nor
is one drawn from a module whose name, taken from its path, has a character a pattern cannot
spell literally: a `:`, or glob syntax (`routes/[id].py`).

What the configuration already declares is not proposed: an attribute a `write-owners` or
`read-owners` target names, and a module an effect boundary forbidding every blocking effect
covers. A kind whose rule the configuration ignores is not proposed either. When the block sets
`src`, no contract is proposed, and no verification run counts the block's rules: a new source
root renames modules and resolves imports heki does not resolve now, so run `heki recommend` again
once it is set. Each target has the
module-part form (`app.models:Order.items`), which never matches another project's names.

The contract trial declares every candidate at once, the ownership contracts with
`unresolved = "report"`, and runs their rules (`MUT201` and `MUT205`, `MUT203` and `MUT206`,
`EFF201`) as `heki check` would: inline suppressions and `per-file-ignores` apply, and the
reports inline suppressions drop are counted apart (a rule `per-file-ignores` turns off for a
file reports nothing there). A candidate the rule of its contract (`MUT201`, `MUT203`, `EFF201`)
reports is rejected and listed with the first report, and so is one that hides a report the
configuration's own contracts make now: a target lists its `__dict__` key, on its class and the
subclasses, for every contract, so a contract with `unlisted-dict-keys = "report"` stops
reporting a write of that key on an object of either (`Store.b` hides the report of a write of
`Child.b` when `Child` derives from `Store`). Reports of `MUT205` or `MUT206`, accesses on objects
of unknown class, reject no candidate: they make it medium (below). Of each kind, the trial
declares the first 32 candidates: `write-owners` with the attributes changed in place first, then
by the modules reading them; `read-owners` by the modules using the class; boundaries by their
functions times the functions calling into them from outside. The configuration's own
`write-owners` contracts take places of the 32.

| Confidence | When |
| ---------- | ---- |
| high | the trial reports nothing for the contract, every Python file below the project root was read, parsed, and analyzed (when the paths given leave part of the root out, the count of the files there got to its end and found none), and no function of a boundary's scopes reaches code heki does not see (a call it does not resolve, a library name without a catalog entry, a function it does not model) |
| medium | the trial reports no violation, but `MUT205` or `MUT206` reports accesses on objects of unknown class (they are listed), files were left out (or the count of them stopped before the end) or could not be parsed, or a scope function reaches code heki does not see (it is listed) |

Medium candidates are in the block too, an ownership one with `unresolved = "ignore"`: heki may
not see every access on objects of unknown class (in the files left out or not parsed, for one)
that the rule of such accesses would report. A high one sets `unresolved = "report"`, unless the
configuration ignores that rule (`MUT205`, `MUT206`): it stays ignored, so the entry sets
`unresolved = "ignore"` and the block does not select the rule. A candidate on a subclass is
left out when the one on its base class is kept: a target covers the subclasses of its class.
Every candidate carries the caveat that accesses inside code heki cannot follow are not seen.

The verification run then applies the whole block, the rules it enables and the contracts it
declares, as `heki check` would; the counts of the block's rules come from it.

## How the advice is kept honest

- Every count comes from the project, through the analysis `heki check` runs: inline
  suppressions, `per-file-ignores`, and the baseline apply as they do there. With the block
  applied, `heki check` prints the reports it promises.
- A contract enters the block only when the trial of its rules reports no violation of it on the
  code as it is, and hides no report the configuration's own contracts make; accesses on objects
  of unknown class make it medium, with `unresolved = "ignore"`. The verification run then
  applies the whole block as `heki check` would.
- How often a rule's reports are right is quoted only from published measurements, with their
  sample, interval, and date; a rule without one is "not measured". No score is computed.
- What keeps a report heuristic comes from the report itself, as the analysis decided its
  tier; a cause the analysis does not record is shown as not recorded, never guessed.
- No library catalog entry enters the block: heki cannot verify what a library call does. What
  a trial entry would make `heki check` print is said as a condition ("if it waits") and kept
  out of every count of reports. `src` enters only after its trial.
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
# app/billing.py
import stripe

async def charge(amount: int) -> None:
    stripe.Charge.create(amount=amount)
```

```python
# tests/test_users.py
def test_load_user() -> None:
    pass
```

<!-- Generated by scripts/build_docs.py from heki recommend on the example above: report. -->

```console
$ heki recommend app
heki recommend (experimental) on .: 4 file(s), 13 function(s)

Reports now  1 shown (1 exact, 0 heuristic), 0 heuristic withheld, 5 heuristic rule(s) off

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

What heki cannot see (no report either way)
  calls of packages heki's catalog does not cover: 1 call(s) of 1 name(s) in 1 package(s)
    name                  calls  async  loops  if it waits  if it sends a query
    stripe.Charge.create      1      1      0  EFF101 1     -

Rules worth enabling here
  rule    reports  exact  consequence  right about the code (published)  real harm  block
  MUT106        1      -  bug          not measured                      -          yes
  OPT110        1      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         no: slower code, not a bug
  No reports here: EFF102, EFF105, EFF106, EFF107, EFF109, EFF110, EFF111, EFF112, EFF113, EFF114, EFF115, EFF116, IMP101, IMP102, IMP104, IMP301, IMP302, MUT101, MUT107, MUT108, MUT111, MUT112, MUT113, MUT115, MUT118, MUT120, OPT103, OPT111, OPT112, OPT120, OPT121, OPT130
  MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, then select it.

Contract candidates
  effect-boundaries  app.report  medium
    3 function(s) in 1 module(s) reach no blocking call heki sees
    trial: EFF201 0
  For every candidate, accesses inside code heki cannot follow (unresolved calls, libraries without a catalog) are not seen.

Notes
  - 1 Python file(s) below the project root were not analyzed (tests/test_users.py): heki takes the analyzed files as the whole project, so what it says about callers, readers, and writers holds for those files only.
  - "if it waits" and "if it sends a query" come from a trial run that gives each library name tried a catalog entry with `effects = ["blocking_net", "db_query"]`: what `heki check` would print if the call waited on the network or sent a database query. heki cannot see what these calls do, so no entry for them is in the block; add one in `[[tool.heki.ecosystem]]` if you know.

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
| `runs` | the analyses the report was computed from: `discovery`, then `source-roots` (the source-root trial) and `hypothetical-catalog` (the trial of library names) when they ran, `contract-trial` when it tried a contract candidate (none when the block sets `src`, or when every candidate is past the places of its kind), and `verification` when the block adds rules or contracts and does not set `src` |
| `reports_now` | the counts of "Reports now" |
| `open_premises` | the counts of "What keeps reports heuristic": by code; by premise or condition and cause, with what the cause states, what closes it, the reports, the reports it is the only open cause of, and the first 50 reports; the sets of causes that reports leave open together |
| `blind_spots` | the counts of "What heki cannot see": the calls of library names without a catalog entry, by package and name, with what the trial run found; the names heki's catalog lacks in packages it covers; the calls heki does not resolve, by function and reason; the source-root candidates and their trials; the functions with an A1 hazard; the totals |
| `rules` | each rule of "Rules worth enabling here": its metadata, the reports it would add, its published measurement, whether the block enables it, and why |
| `contracts` | each contract candidate the block declares: its kind, what it is about, its entry, confidence, evidence, what the trial reported, and caveats |
| `rejected_contracts` | the candidates a run reported for, with the run and its reports |
| `contracts_not_tried` | per kind, the candidates past the trial's places (32 of each kind, the configuration's own `write-owners` contracts taking `write-owners` places), which no trial declared |
| `unmatched_entries` | each entry of "Contract entries that match nothing": the contracts' kind, the key, the entry, the contracts holding it (a name, or `#` and its position), what it names none of, the existing names like it, and `named_at_run_time` when its class family gets attributes named at run time (only with `unmatched-contract-entries = "error"`) |
| `notes` | the notes |
| `config`, `config_toml` | the keys to add or change, as keys and values and as TOML |
| `content` | the report as the text output prints it |

Exit codes: `0` the report was printed; `2` configuration, I/O, or syntax errors (the report
of the other files is still printed, as `heki check` does). It never exits with `1`: it checks
nothing.
