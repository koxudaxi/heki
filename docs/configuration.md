# Configuration

heki reads `[tool.heki]` from `pyproject.toml`, or a `heki.toml` / `.heki.toml` file with the same
keys without the `tool.heki` prefix. Keys are kebab-case, as in Ruff.

```toml
[tool.heki]
target-version = "py312"            # default: inferred from project.requires-python
extend-exclude = ["generated"]      # added to the default excludes
baseline = "heki-baseline.toml"     # Ratchet baseline (heki check, --watch, the server)

[tool.heki.lint]
select = ["MUT", "EFF", "DATA", "OPT"]
extend-select = ["EFF105"]
ignore = ["OPT110"]
per-file-ignores = { "tests/**" = ["EFF"], "scripts/*.py" = ["OPT"] }
relax-async-in-tests = true
fail-on = "warning"                 # warnings and heuristic reports fail heki check too (default: "error")

[tool.heki.lint.severity]
OPT103 = "warning"
```

## Where heki finds the configuration

1. The project root is the nearest directory at or above the first path given (the current
   directory when no path is given) that holds `heki.toml`, `.heki.toml`, or `pyproject.toml`.
   The search stops at the top of the enclosing Git working tree (the directory holding `.git`):
   a configuration above it belongs to another project. Without a configuration file, the root
   is the current directory, as Ruff and ty take it, when it holds the first path; but never a
   directory above the top of the Git working tree (then the root is that top), and outside the
   current directory and any Git working tree it is the first path's own directory (the
   directory given, or the given file's directory). The language server takes its workspace
   folder for the current directory.
2. heki reads the first of `heki.toml`, `.heki.toml`, and `pyproject.toml` in that directory. A
   `pyproject.toml` without `[tool.heki]` still marks the root: its project gets the defaults.
3. `--config <path>` names the file instead. The project root stays where step 1 finds it.

One run checks one project: its configuration applies to every file of the run, also to
directories given from another project. Files given name the project of the first one:
`heki check a/one.py b/two.py` checks `a/`'s project and names `b/two.py` in a warning when it is
not in it. Check projects with their own configurations in separate runs: `heki check a/`, then
`heki check b/`.

### Which tables hold the settings

The file's name decides, as Ruff reads `pyproject.toml` and `ruff.toml`:

- A file named `pyproject.toml` holds the settings in `[tool.heki]` (`[tool.heki.lint]`,
  `[[tool.heki.ecosystem]]`, ...). Its other tables belong to the packaging standards and other
  tools. heki reads only what it infers from them: the default target version
  (`project.requires-python`) and the detected source roots (`project.name` and the workspace
  tables, see [Workspaces and monorepos](#workspaces-and-monorepos)).
- Any other file (`heki.toml`, `.heki.toml`, or the file `--config` names, whatever its name)
  holds the same keys at its top level, without the `tool.heki` prefix (`[lint]`,
  `[[ecosystem]]`).

heki never ignores a setting. These are configuration errors (exit code 2):

- an unknown key in any of heki's tables;
- a `[tool.heki]` table in a file that is not named `pyproject.toml`;
- a heki setting at the top level of a `pyproject.toml` (`[[ecosystem]]` instead of
  `[[tool.heki.ecosystem]]`);
- a value of the wrong type, or one the key does not take.

The error shows the line, says what to write instead, and links the section of this page about
the key: the key a misspelled one looks like, the table a key belongs in, or the type a value
takes, with a line to copy. With `--output-format json`, the same error is the object
`{"error": {"message", "path", "line", "column", "help", "docs", "candidates"}}` on stdout.

```text
error: pyproject.toml:5:1: unknown key `select` in [tool.heki]
  |
5 | select = ["MUT"]
  | ^
  = help: `select` is a key of [tool.heki.lint]: move it there
  = help: for example: `select = ["EFF", "MUT101"]`
  = docs: https://heki.koxudaxi.dev/configuration/#rule-selection-toolhekilint
```

### Module names follow the source roots

Module names are the paths below a source root (`app/api.py` is `app.api` below the project
root, `src/acme/io.py` is `acme.io` below `src`). heki resolves the imports between the
project's modules by these names, and they appear in messages that name a function by its full
name and in the `target`s of [baseline](adoption.md#ratchet-baselines) entries, whose `path`s
are relative to the project root.

- The `src` setting lists the source roots: directories relative to the project root, like
  Ruff's `src` and ty's `environment.root`. A file below several of them is named below the
  deepest one, and a file below none is named below the project root. `src` does not accept
  globs yet (Ruff's does): list the directories.
- Without `src`, heki detects the source roots as ty does: `src`, the directory named after the
  project (`project.name` of the `pyproject.toml`, else the root's own name) when it holds a
  directory of the same name (`acme/acme/`), and `python` (the maturin layout), each when it is
  a directory and not a package (no `__init__.py`). A source root that is a package is imported
  by its own name (`src.helpers`).
- Without `src`, heki also detects the members of the project's uv, Hatch, and PDM workspaces
  (see [Workspaces and monorepos](#workspaces-and-monorepos)).
- Other layouts need `src`: a subdirectory application whose directories are imported as
  top-level packages (`backend/config/env.py` imported as `config.env`) takes
  `src = ["backend"]`, and a `lib/` layout (`lib/matplotlib`) takes `src = ["lib"]`.

#### Workspaces and monorepos

In a workspace, each member is a package of its own, installed from its own directory. Without
`src`, heki reads the workspace tables of the root's `pyproject.toml` and names each member's
modules the way installing the member names them, so a call from one member into another
resolves without configuration:

- the members: uv's `[tool.uv.workspace]` (`members` and `exclude`), the `workspace.members`
  and `workspace.exclude` of every Hatch environment (`[tool.hatch.envs.<env>]`, or
  `[envs.<env>]` in a `hatch.toml` next to it; a member may be a `{path = ...}` table), and
  PDM's `[tool.pdm.workspace]` (`members`). Members are directories relative to the project root
  or globs (`packages/*`; `**` spans directories), and a member must hold a `pyproject.toml`. An
  `exclude` glob drops the members of its own table it matches, and its `*` spans directories
  as uv matches it.
- a member's source roots are the ones detected for the project root, below the member:
  `packages/core/src`, the directory named after the member (`packages/core/core` when it holds
  `core/`), and `python`.
- a member without one (a flat layout) has its packages named below it: `packages/api/acme_api/`
  is the package `acme_api`. Its other files keep their names below the project root
  (`packages/api/main.py` is `packages.api.main`): several members often hold a `main.py` or a
  `scripts/` package, and these would otherwise share a name. A package that several members
  hold is named below the project root for the same reason.

For example, this uv workspace:

```text
pyproject.toml                  # [tool.uv.workspace] members = ["packages/*"]
packages/core/pyproject.toml
packages/core/src/acme_core/io.py
packages/api/pyproject.toml
packages/api/acme_api/server.py # from acme_core.io import wait
packages/api/main.py
```

has the source root `packages/core/src` and the member package `packages/api/acme_api`:
`acme_core.io` is `packages/core/src/acme_core/io.py`, so `heki check` run at the root follows
`server.py`'s call to `wait` into the other member. `heki check --generate-prompt` lists both.

The `src` setting replaces the detection, members included: `src = ["packages/core/src",
"packages/api"]` lists the same roots by hand, with `packages/api` naming all of the member's
files below it. Run heki at the workspace root: run inside a member, the member's own
`pyproject.toml` makes the member the project root, and the other members are outside it.

Without a configuration file the project root is where heki runs (see above): in a repository
without one, `heki check` run in `services/api/` names `services/api/app.py` `app`, and its
`from helpers import slow` finds `services/api/helpers.py`; run from the top of the repository,
the same file is `services.api.app` and the import is not resolved. Put a `pyproject.toml` or a
`heki.toml` in the project's directory to fix the root wherever heki runs.

## Top-level settings

| Key | Default | Meaning |
| --- | ------- | ------- |
| `target-version` | the oldest version `project.requires-python` admits, else `py312` | Python version of the code: `py311` … `py315` (or `3.11` … `3.15`). `--target-version` overrides it. See [below](#the-default-target-version) for the inference and [Scope](scope.md#the-target-python-version) for what the version changes. |
| `required-version` | none | A [PEP 440](https://peps.python.org/pep-0440/) version specifier made of operators and plain releases (`">=0.2.6"`; no pre-, post-, or dev-release suffix) the running heki must satisfy; an older or newer one stops with an error. See [Requiring a heki version](#requiring-a-heki-version). |
| `exclude` | the Ruff defaults (`.git`, `.venv`, `build`, `dist`, `node_modules`, …) | Replaces the excluded paths: globs, matched against the project-relative path and each of its components, and covering everything below a matched directory. |
| `extend-exclude` | `[]` | Adds excluded paths. |
| `src` | the detected source roots | Source roots, relative to the project root: the module names of the files below each are relative to it (`src = ["lib"]` names `lib/pkg/net.py` `pkg.net`). See [Module names follow the source roots](#module-names-follow-the-source-roots). |
| `baseline` | none | Ratchet baseline file, relative to the project root, used when `--baseline` is not given: by `heki check`, `--watch`, and the language server. See [Ratchet baselines](adoption.md#ratchet-baselines). |

### The default target version

Without `target-version`, heki reads `requires-python` from the `pyproject.toml` that holds the
configuration, or, for `heki.toml` or a `--config` file that is not a `pyproject.toml`, from the
`pyproject.toml` at the project root. The same applies to the project name, the declared
dependencies, and the workspace tables: where the configuration file sits does not change them.
It takes the oldest supported version that every clause admits:

| `requires-python` | Target |
| ----------------- | ------ |
| `>=3.11` | `py311` |
| `>=3.11, !=3.11.*` | `py312` |
| `>=3.9, <3.11` (only older versions) | `py311` |
| `>=3.16` (only newer versions) | `py315` |
| `<4` (no lower bound, admits 3.12) | `py312` |
| missing, or not a version specifier | `py312` |

### Requiring a heki version

`required-version` makes a heki that does not satisfy the specifier refuse to run, as Ruff's
`required-version` does. heki checks it before any other key, so a configuration that uses keys
a newer release adds fails with this message on an older heki:

```toml
[tool.heki]
required-version = ">=0.2.6"
```

```text
error: pyproject.toml:2:20: this is heki 0.2.4, but the configuration requires `>=0.2.6`
```

An older heki cannot know which release adds a key it has never heard of. Without
`required-version` (and any heki before the release that added it rejects the key as unknown, like any
other), an unknown key is an error that says only what the table accepts. When the key is not close to a
key of its table (a close one is a typo, and the error offers it), the error also says that the
key may belong to a newer heki, names the running version, and points to `required-version` and
the changelog. It cannot say which release adds the key. `heki version --changelog` prints what
the installed release changed, and the changelog on GitHub lists every release.

### Which files are checked

- Without paths, `heki check` checks every `.py` and `.pyi` file under the current directory.
- Discovery skips hidden files and directories, the paths `exclude` and `extend-exclude` match,
  and the entries of `.gitignore` (inside a Git repository), `.ignore`, and `.hekiignore` files.
- Directories given are analyzed as given: heki takes the files it finds there as the whole
  program, so a chosen set such as `src` without `tests` keeps the claims of the tests out.
- A file given is checked in its project. Whether a report in it holds depends on its callers,
  subclasses, and rebindings anywhere in the project, so `heki check app/api.py` analyzes the
  project root heki finds for the file (as above, from wherever heki runs), shows and fails on
  the reports that touch the file, and fixes only the file, as
  [`--touched`](cli-reference.md#heki-check) does: `heki check <root> --touched app/api.py`.
  Several files make one run over the project of the first. With directories, the directories
  are analyzed and the files select what is shown. A file the project leaves out (excluded,
  ignored, or not a `.py` or `.pyi` file) is named in a warning. `--update-baseline` and
  `--watch` take directories.
- A file outside any project, with no configuration file and no Git working tree above it, is
  analyzed alone, whatever its extension.
- A directory below the project root, given or the current directory, is analyzed as given. When
  its files import project modules it leaves out, the run misses what they do, and says so in a
  note naming them.

## Rule selection: `[tool.heki.lint]`

| Key | Default | Meaning |
| --- | ------- | ------- |
| `select` | the exact rules that are not opt-in, and the contract rules | Rules to enable instead of the defaults. An empty list enables no rule before `extend-select`, as in Ruff. |
| `extend-select` | `[]` | Rules to add. |
| `ignore` | `[]` | Rules to disable. |
| `per-file-ignores` | `{}` | Glob → rules disabled in matching files (project-relative POSIX paths). As in Ruff, `*` also matches `/`: `scripts/*.py` covers `scripts/tools/worker.py` too. |
| `severity` | `"warning"` for the rules that report slower code (`EFF102`, `EFF103`, `EFF105`, `DATA101`, and the `OPT` rules) and for `MUT110`, which reports a bug that is rarely harmful; `"error"` for the others, which report bugs that can be critical (`EFF101`: a blocked event loop holds up every other task) | Rule → `"error"` or `"warning"`. It changes the output and the SARIF level. By default only exact errors make `heki check` fail: a heuristic report counts as a warning, whatever its severity; `fail-on = "warning"` makes warnings and heuristic reports fail it too. |
| `relax-async-in-tests` | `true` | Skip `EFF101`, `EFF105`, and `EFF106` in test code as pytest finds it: `test_*` functions and the functions nested in them, `test_*.py` and `*_test.py` files, and files under `test/` or `tests/`. |
| `heuristic` | `false` | Whether the heuristic rules that are not opt-in run by default and for a prefix or `ALL`, as the exact rules do. `--heuristic` and `--no-heuristic` override it. |
| `fail-on` | `"error"` | The least severity whose reports make `heki check` (and `--watch`) exit with code 1: `"error"`, or `"warning"` to fail on warnings too. A heuristic report counts as a warning, whatever its severity: it can be wrong about the code, so at `"error"` only exact errors fail the run. Warnings and heuristic reports are reported either way, and the language server shows them. `--fail-on` overrides it. A report of a contract with [`fail = true`](#failing-on-a-contract) fails the run at either value. |
| `report-unused-suppressions` | `false` | Report the codes of suppression comments that suppressed no report, and the baseline entries that matched none. See [Unused suppressions](#unused-suppressions). |
| `require-suppression-codes` | `false` | Report the `# heki: ignore` comments that name no rule. See [Suppressions that name no rule](#suppressions-that-name-no-rule). |
| `unmatched-contract-entries` | a warning, in a run over the whole project | `"warning"`, `"error"`, or `"ignore"`: what `heki check` does with a contract entry that matches nothing in the analyzed files. An error fails the run. See [Entries that match nothing](#entries-that-match-nothing). |

- A rule is named by its code (`EFF101`), its name (`async-blocking-call`), a code prefix
  (`EFF`, `MUT2`), or `ALL`. An unknown rule is a configuration error.
- Every rule is exact or heuristic ([Rule Precision](precision.md)). The defaults, a prefix, and
  `ALL` enable the exact rules that are not opt-in, and the contract rules; with
  `heuristic = true` they also enable the heuristic rules that are not opt-in, as Ruff's
  `preview` does. Its code or name enables a
  heuristic rule either way.
- An exact rule's report that did not establish every premise of its claim (an `OPT` report
  whose rewrite is not proven) is heuristic, and is shown where a heuristic rule would run: with
  `heuristic = true`, or when its code or name is selected.
- A run of `full` or `concise` output that leaves heuristic rules off, or heuristic reports out,
  says how many.
- A prefix or `ALL` never enables the opt-in rules (`EFF105`, `EFF106`, `EFF107`, `EFF109`,
  `EFF110`, `EFF112`, `EFF114`, `EFF115`, `EFF116`, `IMP101`, `IMP102`, `IMP104`, `IMP301`,
  `IMP302`, `MUT101`, `MUT103`, `MUT106`, `MUT107`, `MUT108`, `MUT111`, `MUT112`, `MUT113`,
  `MUT115`, `MUT120`, `OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`): they need
  their exact code or name. In `ignore`, a prefix covers every rule it matches.
- `heki check --include-size-dependent` also runs the opt-in rules whose size-dependent rewrites
  it applies (`OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`), as if `--extend-select` named
  them; `ignore` still removes them.
- The command line wins: `--select` replaces `select`, `--extend-select` and `--ignore` add to the
  configured lists, and `--heuristic` / `--no-heuristic` replace `heuristic`.

```toml
[tool.heki.lint]
heuristic = true   # also the heuristic rules that are not opt-in
```

## Contracts

Contracts declare what no type can say. Each kind is an array of tables with its own rule:

| Contract | Declares | Rule | Certainty |
| -------- | -------- | ---- | --------- |
| [`write-owners`](#write-owners) | who may write an attribute | `MUT201`; `MUT205` with `unresolved = "report"` | heuristic |
| [`read-owners`](#read-owners) | who may read an attribute | `MUT203`; `MUT206` with `unresolved = "report"` | heuristic |
| [`call-owners`](#call-owners) | who may call a function or construct a class | `MUT204`; `MUT207` with `unresolved = "report"` | heuristic |
| [`effect-boundaries`](#effect-boundaries) | what a scope must not reach: I/O, writes, reads, or calls | `EFF201` | heuristic |
| [`readonly-returns`](#readonly-returns) | results every caller shares | `MUT202` | heuristic |
| [`invalidation`](#invalidation) | writes that must be announced | `MUT208` | heuristic |
| [`import-boundaries`](#import-boundaries) | which modules a module may import (experimental) | `IMP201`; `IMP202` with `unresolved = "report"` | heuristic |

- An `*-owners` contract protects its `targets` and lists who may touch them. A
  `*-boundaries` contract is drawn around the code it names (`scopes`, `importers`) and lists
  what that code must not reach or import.
- These rules report nothing until a contract exists. The results of `functools.lru_cache` /
  `functools.cache` functions are checked without one by `MUT114` (`cached-result-mutation`).
- Declaring a contract selects its rule, whatever its certainty.
- Their reports are heuristic, so they count as warnings for
  [`fail-on`](#rule-selection-toolhekilint) and do not fail the run by default: to make a broken
  contract fail `heki check`, the pre-commit hook, and CI, set `fail = true` in its table
  ([below](#failing-on-a-contract)).
- Every name in a contract but `import-boundaries`, whose names are
  [module patterns](#import-boundaries), is a dotted pattern. It matches the full name or any dotted suffix
  (`DataModel.fields` matches `pkg.model.DataModel.fields`, not `pkg.BigModel.fields`), and it
  may be a glob (`*`, `?`, `[...]`, `{a,b}`). An alternative may be empty, as in a shell's brace
  expansion: `pkg.run:outer{,.*}` names `outer` and every scope nested in it (`outer.<locals>.inner`,
  a class defined in it, their methods), where `pkg.run:outer` names `outer` alone. It works
  wherever a configured text is a glob, module patterns, `exclude`, and `per-file-ignores`
  included.
- A glob's `*` spans dots (`pkg.parser.*` covers the functions of every module under
  `pkg.parser`), except in the last segment of a target, which names the attribute, function,
  method, or class: `pkg.*._*` names the private members of everything in `pkg`, and no function
  of a private module whose own name is public; `pkg.api_*` names `pkg.api_commit`, not
  `pkg.api_helpers.api_reset`.
- A name with a module part, `module:qualname`, names definitions of the project modules the
  part names, never of their submodules. The module part matches the whole module name, exactly
  or as a glob (`pkg.*:run`), not a suffix; the qualified name after the colon matches the whole
  name inside the module, a `*` spanning dots. `pkg:*` names everything `pkg/__init__.py`
  defines (its functions, classes, methods, nested functions, and module body `<module>`), where
  `pkg.*` also covers `pkg.sub.user.sub_user`; `pkg.sub:Store.*` names the methods of `Store` in
  `pkg.sub`. In a `Class.attr` target the module part belongs to the class
  (`pkg.model:DataModel.fields`), and a call target with one is matched whole (`pkg:*` names
  every function, method, and class of `pkg`). Such a name never matches an external symbol, and
  `unresolved = "report"` counts its last name (`decide` of `pkg:decide`). A module part that
  names no project module is no error: the name matches nothing, which `heki check` warns
  about ([Entries that match nothing](#entries-that-match-nothing)).
- A report of a glob target names the member it matched (`reads
  `pkg.types.DataType._flag``); one report standing for several members lists the others.
- An access matching several targets of one contract (a target on a class and one on its
  subclass, a protocol's member and its implementation's attribute) is one report of the
  contract: it names the target configured first among them, and lists the others (`(also
  `A.items`)`).
- A `Class.attr` target covers the subclasses of the class too.
- A private name of a class (`__cache`, `__run`: two leading underscores and not two trailing)
  is the member Python's name mangling makes of it (`_Parser__cache` in class `Parser`), and a
  name may spell it either way: `pkg.Parser.__run` and `pkg.Parser._Parser__run` (or
  `pkg.Parser._Parser__*`) name the same method. An access is matched as Python runs it: inside
  `Parser`, `self.__run` reaches `_Parser__run`; outside, a receiver typed `Parser`, a
  subclass's `self`, or `type[Parser]` reaches it as `_Parser__run`. A subclass's own private
  name (`self.__run` in `JsonParser`, the member `_JsonParser__run`) is another member, and
  overrides nothing of its base. With `unresolved = "report"`, the last name of such a target
  counts in both spellings (`__run`, and `_Parser__run` for a class the target names); a
  target without a class (`__run`) names the member of any class (`other.__run()` in class
  `Driver` calls `_Driver__run`).
- A contract heki cannot use is a configuration error: a target that is not `Class.attr`, an
  invalid glob, a name with more than one colon or an empty module or qualified name, call
  targets whose globs are too many to match together (tens of thousands of complex ones), an
  argument list heki cannot read, an invalid glob in `allowed-protocols`, an unknown group, a
  group in `relative-imports`, or an `ignored-imports` entry without `->` in `import-boundaries`, or
  an unknown value of `forbid`,
  `fresh-objects`, `unresolved`, `references`, `attribution`, `own-receiver`,
  `unlisted-dict-keys`, `type-checking`, or `reach`.
- With `unresolved = "report"`, a contract also reports the accesses it cannot tie to a class,
  each under its own rule (`MUT205` writes, `MUT206` reads, `MUT207` calls): an attribute or
  method named like a target, accessed on an object of unknown class (an unannotated parameter,
  a loop variable, an untyped call result, `Any`, a protocol whose implementations heki cannot
  list). A baseline counts them apart from the violations the typed rules prove. An owner
  contract with it also judges a function some code hands on as a value (a display, a class
  attribute, an argument, a return: `HANDLERS = [helper]`, `REQUIRE = staticmethod(helper)`)
  in that function, as if it had no callers: such a function has callers heki does not see, so
  its allowed callers do not allow what it does.
- An attribute named through a string (`setattr`, `getattr`, `__dict__`, `vars`) is followed
  when the string is a constant: see [Dynamic names](scope.md#dynamic-names).

### Defaults for the tables of a kind

A configuration with many tables of one kind repeats the same values in each. A defaults table
per kind, named `<kind>-defaults` (`[tool.heki.call-owners-defaults]`,
`[tool.heki.effect-boundaries-defaults]`, and one for each of `write-owners`, `read-owners`,
`call-owners`, `effect-boundaries`, `readonly-returns`, `invalidation`, and `import-boundaries`),
sets the keys that every table of its kind takes when it does not set them:

```toml
[tool.heki.call-owners-defaults]
attribution = "lexical"
references = "reads"
subclasses = false

[[tool.heki.call-owners]]
name = "commit"
targets = ["pkg.db.Session.commit"]
allowed-callers = ["pkg.db.*"]

[[tool.heki.call-owners]]
name = "rollback"
targets = ["pkg.db.Session.rollback"]
allowed-callers = ["pkg.db.*"]
references = "ignore"
```

- The result is the configuration with every table written out: both tables above have
  `attribution = "lexical"` and `subclasses = false`, and `rollback` keeps its own
  `references`. `heki check`, `heki inspect`, and `heki recommend` read the effective values.
- A table's own key overrides the default, an empty list too (`allowed-callers = []` is a list
  of no callers). A list is replaced whole, never merged with the default's.
- `name` and `targets` name one contract, so they are errors in a defaults table. Any other key
  of the kind may be defaulted, `fail` among them: `fail = true` in `[tool.heki.call-owners-defaults]`
  makes the reports of every `call-owners` table fail the run, and a table with `fail = false`
  keeps its reports advisory.
- A defaults table is one table, not an array of tables. With no table of its kind it changes
  nothing. In a file that is not a `pyproject.toml` it is written without the `tool.heki`
  prefix (`[call-owners-defaults]`).
- A table you add to the file from `heki recommend` takes the defaults like any other, and the
  recommender tries it with them.
- An error about a value of a defaults table is reported at the defaults table.

### `write-owners`

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields", "DataType.data_types", 'DataModelFieldBase.extras["init"]']
allowed-writers = ["pkg.parser.generation.*", "Store.*"]
exempt-methods = ["__deepcopy__", "copy_deep"]
forbid-cached-property-on = ["DataModel"]
message = "use GenerationStore.set_fields() instead"
fresh-objects = "allow"
unresolved = "report"
```

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | `Class.attr` attributes, or `Class.attr["key"]` for one entry of the container an attribute holds. |
| `allowed-writers` | `[]` | Functions allowed to write the targets. |
| `forbidden-writers` | `[]` | When set, only writes by these functions are reported: see [Allowed and forbidden functions](#allowed-and-forbidden-functions). |
| `exempt-methods` | `[]` | Construction and copy methods allowed to write the targets. A plain name (`__deepcopy__`) matches any method of that name; a name with a dot or a glob (`DataModel.copy_deep`, `create_*`) is a pattern, as in `allowed-writers`. |
| `forbid-cached-property-on` | `[]` | Classes on which defining a `@cached_property` is itself reported. |
| `message` | none | The help of every report of the contract: where to write instead. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |
| `fresh-objects` | `"allow"` | `"allow"` or `"report"`: whether a write that constructs the object it writes is reported. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the writes the contract cannot tie to a class are reported under [`MUT205`](rules/unresolved-guarded-write.md). |
| `attribution` | `"call-graph"` | `"call-graph"` or `"lexical"`: which function a write is judged in. See [How contracts report](#how-contracts-report). |
| `own-receiver` | `"report"` | `"report"` or `"allow"`: whether a write through the receiver of the method it is judged in (`self`, `cls`, `super()`) is allowed. See [How contracts report](#how-contracts-report). |
| `allowed-protocols` | `[]` | Project protocols (class patterns, their subclasses included): a write through a value typed with one that declares the attribute, itself or through its protocol bases, is allowed. See [How contracts report](#how-contracts-report). |
| `protocol-subclasses` | `true` | `false`: an `allowed-protocols` pattern names the protocols it matches, not their subprotocols (a protocol deriving from an allowed one stays judged). |
| `unlisted-dict-keys` | `"ignore"` | `"ignore"` or `"report"`: whether a `__dict__` key of a target's class that no `write-owners` target names is reported when it is written; with `unresolved = "report"`, also on an object of unknown class (`MUT205`). |

A container entry (`Class.attr["key"]`):

- matches the writes of that constant key (a literal or a string constant), also by a store
  operation a call passes the key to (`set_extra(field, "init", v)` writing `field.extras[key]`,
  reported at the call), not a write of the whole container or with a computed key;
- is reported once, under the entry, when its container is a target too;
- in an effect boundary, makes a write of the container under a key heki cannot tell a report of
  its own ("under a key heki cannot tell"), unless the boundary allows the whole container.

A write into the entries of a target's container (`model.meta["kw"][name] = v`) is a write of
the target. So is a write through a local alias of the container (`kw = model.meta["kw"]`), also
after an augmented assignment that changes the container in place and so keeps it
(`kw |= other`, `items += more`, `-=`, `&=`, `^=` on a set); one on any other value
(`count += 1`) binds a new object.

With `fresh-objects = "allow"`, a write is no violation when it constructs the object it writes:

- the object was allocated in the same call chain: by calling its class, by `object.__new__`
  (`object.__new__(Node)`), by a copy, or by a project function returning a new object;
- nothing can hold it yet: no store, `yield`, closure, or call that may keep it ran before the
  write.

A shallow copy's containers are the original's: only rebinding its attributes constructs it.

A project function returns a new object at a call when every value it returns is one, or, on
some paths, an element of an optional parameter (an item, `get`, `pop`, `setdefault`, or an
iteration step of it) that the call leaves at a default without elements or passes such a
constant: `None`, a bool, a number, or an empty string or tuple. `copy_tree(node)` of
`def copy_tree(node, replacements=None)` returning `replacements[id(node)]` or a copy gives a new
object; `copy_tree(node, table)` does not, nor does a call where the default is a dict display
shared by every call (`memo={}`). A call of a function every `return` of which returns one of its
parameters (`def with_origin(field, kind): ...; return field`) gives the object it hands that
parameter: a new one stays new, and the input stays the input.

A store a function makes only where tests of its parameters pass (`if register:`,
`if registry is not None:`, also inside `and`, `or`, and `not`) keeps nothing at a call whose
arguments fail them: a constant, the literal default of a parameter the call leaves out, or a test
the caller has already made of the same input. `copy_tree(node, register=False)` of
`def copy_tree(node, *, register=True)` that calls `copied.register()` under `if register:` gives
a new object, and `attach(copied, keep=False)` of a helper storing its parameter under `if keep:`
leaves `copied` new; `copy_tree(node)` does not. A test of the caller's own parameter is decided at
the calls of the caller. A function records at most four sets of at most four tests for each such
store; past them it keeps the object whatever it is passed. A write a helper makes to its own
parameter after such a store still counts the store.

In an effect boundary, a write a reached function makes to the object one of its parameters
holds constructs that object when every call reaching the function hands it a new object that
nothing kept yet, as the same write in the caller would: `set_flag(copy.copy(node))` with
`def set_flag(node): node.flag = True`. A call handing it the scope function's input, or any
other object, is reported, at that call.

With `unlisted-dict-keys = "report"`, `MUT201` also reports a constant key written through the
namespace of an object of a class a target covers (`field.__dict__[KEY] = v`,
`field.__dict__.setdefault("_cache", v)`, `del vars(field)["k"]`) when no target covering that
class names the key, in this contract or in any other `write-owners` contract, so a new cache key
needs a target first. A target names the keys its attribute pattern matches (`Field._*` names
`_cache`), and an entry target names its container. A key another contract names is that
contract's to guard: its writes are judged by that contract's allowance alone. A contract with only
`forbidden-writers` allows no writer, so it names no key. The contract's
allowance applies to an unlisted key as to a target write. A key a loop takes from a tuple,
list, or set of string constants, or from a module's tuple of them, writes each of them, and so
does the key of each pair a generator of them gives `update`. A key computed at run time has no
name to check (`MUT205`, with `unresolved = "report"`), nor has a copy of the keys of an instance's
namespace or of the fields its class declares, whose keys exist already, and neither has a
write on an object heki cannot type, a helper's parameter that no annotation types included:
with `unresolved = "report"` too, `MUT205` reports such a key (below). A key every function may
write is listed by a contract that allows every writer:

```toml
[[tool.heki.write-owners]]
name = "bookkeeping"
targets = ["pkg.model.Field._derived_from", "pkg.model.Field._seen_*"]
allowed-writers = ["*"]
```

With `unresolved = "report"`, `MUT205` reports:

- an attribute named like a target, written on an object of unknown class;
- a computed attribute name (`setattr(obj, name, v)`, `obj.__dict__[name] = v`,
  `obj.__dict__.update(...)`, `vars(obj) |= other`) written on a target class, or on an object
  of unknown class, where it may be any target;
- with `unlisted-dict-keys = "report"`, a constant `__dict__` key that no target names for some
  class the contract's targets cover, written on an object of unknown class, where it may be a
  new key of that class. A helper whose callers hand it only objects of classes no target covers
  is not reported, and neither is a class's own initialization through a helper it passes `self`
  to.

### `read-owners`

```toml
[[tool.heki.read-owners]]
name = "reference-children"
targets = ["pkg.reference.Reference.children"]
allowed-readers = ["*GenerationIndex*", "*GenerationStore*"]
exempt-methods = ["__deepcopy__"]
message = "read children through GenerationIndex instead"
unresolved = "report"
```

The read side of `write-owners`. A read is `ref.children`, a read through a local alias,
`getattr(ref, "children")`, or `ref.__dict__["children"]`, also through a class object, which
reads the class attribute (`Reference.children`, `cls.children`, a value typed
`type[Reference]`).

A read, or a call (`call-owners`, and an effect boundary's `call-targets` and
`allowed-callees`), through a value typed with a project protocol that declares the member is
judged by the protocol, the type the code uses it through: `def f(h: Host): h._m` reads
`Host._m`, not `_m` of the classes implementing `Host`, and a private method of an
implementation called through the protocol keeps what it reads and calls. A write through such
a value still writes the implementations' attribute. `allowed-protocols` allows the reads, calls,
and writes through the protocols it names, also through a class object typed `type[P]` of such a
protocol (see [How contracts report](#how-contracts-report)).

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | `Class.attr` attributes. A container entry (`Class.attr["key"]`) is a write target only, and a configuration error here. An entry can also name attributes of modules outside the project ([below](#symbols-outside-the-project)). |
| `allowed-readers` | `[]` | Functions allowed to read the targets. |
| `forbidden-readers` | `[]` | When set, only reads by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to read the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the reads of a target's attribute name on an object of unknown class are reported under [`MUT206`](rules/unresolved-guarded-read.md). A target ending in a glob (`Context._*`) matches only the names a class it names, or a project subclass, has from project code ([below](#globs-on-objects-of-unknown-class)). |
| `attribution` | `"call-graph"` | `"call-graph"` or `"lexical"`, as in `write-owners`. |
| `own-receiver` | `"report"` | `"report"` or `"allow"`, as in `write-owners`. |
| `allowed-protocols` | `[]` | As in `write-owners`, for a read or a call through the protocol. |
| `protocol-subclasses` | `true` | As in `write-owners`. |

`MUT203` does not see a read through a parameter without a project-class annotation; with
`unresolved = "report"`, `MUT206` reports it unless every caller binds the parameter to an
instance of a project class outside the targets' classes.

### `call-owners`

```toml
[[tool.heki.call-owners]]
name = "reference-mutators"
targets = ["DataType.replace_reference", "pkg.util.reset_cache", "FinalFacts"]
allowed-callers = ["pkg.parser.generation.*"]
exempt-methods = ["__deepcopy__"]
message = "use a GenerationStore operation instead"
unresolved = "report"
```

The call side of `write-owners`.

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | Functions, methods, or classes. A `Class.method` target covers the method's overrides in subclasses; a class target means constructing the class or a subclass, also through a `functools.partial` object holding it and through a library method building an instance of its receiver's class (a pydantic model's `model_construct`, `model_validate` of a dict, `model_copy`). A target may end in an argument list (`DataType(is_dict=True)`, below). An entry can also name functions and classes outside the project ([below](#symbols-outside-the-project)). |
| `subclasses` | `true` | `false`: a class target means constructing exactly the class, not a subclass (an exception class whose subclasses anyone may raise). `Class.method` targets keep covering overrides. |
| `allowed-callers` | `[]` | Functions allowed to call the targets. |
| `forbidden-callers` | `[]` | When set, only calls by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to call the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the calls of a method named like a target (its last name) on an object of unknown class are reported under [`MUT207`](rules/unresolved-guarded-call.md). A target ending in a glob matches only the names a class it names, or a project subclass, has from project code ([below](#globs-on-objects-of-unknown-class)). |
| `references` | `"ignore"` | `"ignore"`, `"calls"`, `"calls-and-bases"`, or `"reads"`: whether a reference handing a target on to be called later counts as a call of it (`[decide]`, `return decide`, `partial(decide)`, `field(default_factory=Rule)`, `obj.callback = self.decide`), with `"calls-and-bases"` a class statement naming a class target among its bases too (`class Derived(Base)`), and with `"reads"` every other use of a target's value as well (`Keys.of(...)`, `Code.OK`, `isinstance(x, Keys)`, `case Keys():`, `tuple[Keys, int]`). |
| `attribution` | `"call-graph"` | `"call-graph"` or `"lexical"`, as in `write-owners`. |
| `own-receiver` | `"report"` | `"report"` or `"allow"`, as in `write-owners`. |
| `targets-call-each-other` | `"allow"` | `"allow"` or `"report"`: whether a target may call the other targets of the contract. `"report"` judges each target like the one target of a contract with the same keys, so one contract listing independent targets reports what one contract per target reports for every call whose candidates all belong to one target. A call that may reach several targets (`obj.reset()` on `obj: Base` with `A.reset` and `B.reset` listed) is judged once for each target it may reach, with only that target guarded, and is reported when any of those judgements reports it; no contract of one target judges that call whole. |
| `allowed-protocols` | `[]` | As in `write-owners`, for a read or a call through the protocol. |
| `protocol-subclasses` | `true` | As in `write-owners`. |

- A call is reported only when every function it may call (each candidate in the class
  hierarchy) is a target: `text.replace(...)` on a `str` is never a call of
  `DataType.replace_reference`.
- Bound-method aliases (`replace = dt.replace_reference`) and `getattr(dt, "replace_reference")`
  with a constant name are followed.
- The targets of a contract may call each other: a call of the guarded operation is checked where
  it is made, so what the operation does inside is part of it. That suits a contract whose targets
  make up one operation. For a contract listing independent targets, set `targets-call-each-other =
  "report"`: a target is then allowed to call itself only, and every call of another target of the
  contract is judged like a call of the one-target contract (a call of `pkg.core.a` by the target
  `pkg.core.c` is reported unless `allowed-callers` names `c`).
- With `references = "calls"`, a target handed on is reported where it is named ("`f` hands on
  `pkg.core.decide`"; a name an import or the module body binds, read in the scope binding it, at
  the statement handing it on; a name a class body, a function, a lambda, or a comprehension
  nested in a function reads from it, a local import included, at the name): passed to a call, stored in an attribute, an item, or a global, returned,
  yielded, raised, or held by a display that is. A use that only names it is no reference: an
  argument of `isinstance`, `issubclass`, another builtin that only inspects its arguments
  (`print`, `getattr`, `type`), `super`, `typing.cast`, `typing.TypeVar`, `typing.NewType`, or
  `contextlib.suppress`; an `except` clause, a class's bases and keywords, a default or an
  annotation of a function; a subscript (`Optional[Rule]`), a comparison, a receiver
  (`Rule.of(...)`), and a module binding another name to it (an import, `run = core.decide`). A
  callable a builtin calls once per item (`sorted(xs, key=decide)`) is a call already.
- A target ending in an argument list, `name(parameter=value, ...)`, names the calls of `name`
  that pass each value: `"DataType(is_dict=True)"`, `'container_flags(kind="dict")'`. A value is
  `True`, `False`, `None`, an integer, or a string in single or double quotes (no escapes). A call
  passes it when heki establishes that the argument bound to the parameter is that constant (a
  literal, a string constant bound once, such as a module's `Final` name, or a module's number or
  boolean bound once, as `EFF101` evaluates them: `LIMIT = 0`, also imported (by the module or by
  a function) or read as an attribute of its module (`settings.LIMIT`), unless a module rebinds
  it; an imported name the importing module binds again in a function declaring it
  `global`, in another module-level store, or with `del`, or that a star import may rebind, is no
  constant there): passed by keyword (also to a class without a project `__init__`, a dataclass or a
  pydantic model), by position where every function the call may run binds that position to the
  parameter, or left out where every such function's default is the value and only `True`,
  `False`, `None`, `0`, or `""` (heki keeps defaults by class and truthiness). A keyword binds as
  Python binds it: never a positional-only parameter (with `def f(kind=False, /, **kwargs)`,
  `f(kind=True)` leaves `kind` false), and a keyword naming no parameter passes its value under its
  name into `**kwargs`; the receiver of a method call is its first position. A `**` operand that is
  a mapping the function builds, which nothing changes (a name holding it is only unpacked with
  `**`, merged into another mapping, bound to another name, tested, or returned), passes its
  entries as keywords: a dict display with string constant keys (literals, or string constants as
  above), `DataType(**{"is_dict": True})`, `flags = {"is_dict": True}; DataType(**flags)`; a merge
  of mappings (`{**a, **b}`, `dict(a, is_dict=True)`, `a | b` of two mappings the function builds),
  which passes the last entry of each key, a mapping heki does not build (a parameter) replacing
  every key before it; and a mapping built on several paths (`flags = a if c else b`), which passes
  a key where every path passes the same constant for it. Calling a `functools.partial` object the
  function makes (through copies) calls what it holds, with the positions it holds first and its
  keywords, which a keyword the call writes replaces: after `make = partial(DataType,
  is_dict=True)`, `make()` constructs `DataType(is_dict=True)`, reported at `make()`, where the
  class is constructed (a call unpacking a mapping with `**` over keywords the object holds is not
  followed). A pydantic model's `model_construct` passes its keywords to the fields,
  `model_validate` the entries of the dict it is passed, and `model_copy` those of `update`. A
  call that raises `TypeError` instead of running the function passes nothing: two keywords of one
  name (written and in such a display, or in two displays), a keyword binding a parameter a
  position or the receiver binds too, a keyword naming no parameter of a function without
  `**kwargs`, a position (the receiver included) past the parameters of a function without
  `*args`, or a parameter without a default left unbound where no `*` unpacking or `**` unpacking
  of another mapping may bind it. Any other argument (a variable, a call, `*` unpacking, `**`
  unpacking of another mapping) is not established, and the call is not reported. A reference passes no arguments, so it never matches. A target with an argument list
  names no symbol outside the project; `unresolved = "report"` matches its last name before the
  list, whatever the arguments of a call on an object of unknown class.
- With `references = "calls-and-bases"`, a class statement passing a class target as a base is
  reported too ("`<module>` subclasses `pkg.core.Base`"), at the base, or at the class statement
  for a name an import or the module body binds: the class it makes holds the base, and
  constructing it runs the base's initialization unless it overrides it. A base is the value the
  statement passes, typed where the statement runs as any value is (a parameter, a local, or a
  class-body name of the imported class's name is another value), and matched as a construction
  is (with `subclasses = false`, the class itself only); a keyword (`metaclass=Base`) is no base.
- With `references = "reads"`, everything `"calls-and-bases"` counts is reported as before, and
  every other use of a target's value is reported too ("`f` reads `pkg.core.Keys`"), where it is
  named as a hand-on is: the object of an attribute read, store, or deletion and a method's
  receiver (`Keys.of(names)`, `Keys.__annotations__`, `Code.OK`), an argument of a call that only
  names it (`isinstance(value, Keys)`, `cast(Keys, x)`, `TypeVar("T", bound=Keys)`), a subscript
  or its index (`tuple[Keys, int]`, `Optional[Keys]`), an operand (`f is decide`, `Keys | None`),
  a tested condition (`if decide:`), an iterated, awaited, formatted, or `with` value, a dict
  display's key, the class of a class pattern (`case Keys():`), the types of an `except` clause
  an exception may reach, a decorator, a default of a function or lambda, a keyword of a class
  statement, and the cause of `raise ... from`, also through copies and the displays holding it
  (`isinstance(value, (Keys, Code))`, a test of a module-level display holding it). No read: a
  call (a call of its own), an annotation, evaluated or not (`list[Keys]`, `Keys | None`), the
  values Python evaluates only when asked (a `type` statement's value, the bounds and defaults of
  type parameters), an import, a name bound to the target by itself (`Alias = Keys`, a local, a
  `nonlocal` cell), whose own uses count, and a value nothing uses (`Keys` as an expression
  statement). A load handing a target on is reported as a hand-on only. A read is matched as a
  construction is, so a subclass counts unless `subclasses = false`; an instance of the class is
  no read of it.

### Symbols outside the project

The `targets` of `call-owners` and `read-owners` can also name functions, classes, and module
attributes outside the project, as an effect boundary's `call-targets` and `read-targets` do, so
one contract guards the private members of both:

```toml
[[tool.heki.call-owners]]
name = "no-private-calls"
targets = ["pkg.*._*", "<external>._*"]
forbidden-callers = ["pkg.parser.*"]

[[tool.heki.read-owners]]
name = "no-private-reads"
targets = ["sys._getframe"]
forbidden-readers = ["pkg.parser.*"]
```

- A dotted name starting with the top-level module names that symbol: `sys._getframe` matches a
  call of `sys._getframe`, also through `from sys import _getframe`, an alias, or a project
  module re-exporting it (an access is judged at the symbol heki resolves it to). A glob in the
  last name names the members it matches (`sys._*`). A dotted suffix (`_getframe`), a leading
  glob (`*._getframe`), a module part, or an argument list names project code only.
- `<external>.<name>` names the members of every module and class outside the project whose last
  name `<name>` matches: `<external>._*` the private ones. A glob there leaves out the names that
  are public API although they start with an underscore: dunder (`__name__`) and sunder
  (`_missing_`) names, and `_asdict`, `_field_defaults`, `_fields`, `_make`, and `_replace` of
  `collections.namedtuple`, the names ruff's `flake8-self` does not report either. A name written
  out (`<external>._replace`) matches as written. `<external>` takes one name after it; any other
  spelling is a configuration error.
- A `call-owners` target counts the calls of the symbol, a `read-owners` target every other load
  of it: `return sys._getframe`, `getframe = sys._getframe`, and an import binding it (`from sys
  import _getframe`, at the import, in the function or the module body running it). A name
  loaded only to be called there (`_getframe(1)`) is a call, not a read, as `sys._getframe(1)` is.
  A reference handing an outside symbol on is no call of it, whatever `references` says.
- Only a call or a read heki resolves counts: a name of unknown origin, or an attribute read by
  name (`getattr(sys, name)`), is not reported. With `unresolved = "report"`, an `<external>`
  entry matches nothing on an object of unknown class; a written last name (`sys._getframe`)
  matches its name there, as any target's last name does.
- The rest of the contract applies as for project targets: the allowed and forbidden functions,
  the exempt methods, `attribution`, and the reports of private helpers at their callers. A
  class's own initialization and `own-receiver` allow none of these accesses: an outside symbol
  is no member of the class making them.

### Globs on objects of unknown class

With `unresolved = "report"`, a `read-owners` or `call-owners` target names the reads or calls of
its last name on objects of unknown class (`MUT206`, `MUT207`). A last name that is a glob
(`Context._*`) matches there only the names a class the target names, or a project subclass of
one, has from project code: the names the class or a project base declares (in its body, set on
its instances by its methods or its module, or stored on the class object), and the names the
code writes on objects typed with one of them or on objects of unknown class. A name only a base
outside the project gives the class is that base's API, which any object may have: with
`targets = ["Context._*"]` and `class Context(NamedTuple)`, an untyped `ctx._replace(...)` is not
reported, and an untyped `ctx._secret()` is when `Context` defines `_secret`.

- A class whose module stores attributes heki cannot name (`setattr(self, name, v)`), or on
  whose instances the code writes names computed at run time, may have any name.
- A glob naming no holder (`_*` among the `call-owners` targets) matches the names of every
  project class; one naming only modules (`pkg.util.*`) or symbols outside the project matches
  none: no object of unknown class has them as members.
- A written-out last name (`Context._replace`) matches by name, and so do the writes `MUT205`
  counts: a write may add the member.

### Allowed and forbidden functions

`allowed-writers`, `allowed-readers`, and `allowed-callers` name the functions a contract
allows. `forbidden-writers`, `forbidden-readers`, and `forbidden-callers` name where it applies
instead, so a layer rule ("the parser must not read `Reference.children`") needs no list of every
other layer:

```toml
[[tool.heki.read-owners]]
name = "parser-reads"
targets = ["Reference.children"]
forbidden-readers = ["pkg.parser.*"]
allowed-readers = ["pkg.parser.index.*"]
```

| `allowed-*` | `forbidden-*` | Reported |
| ----------- | ------------- | -------- |
| unset | unset | every access |
| set | unset | accesses outside the allowed functions |
| unset | set | accesses by the forbidden functions |
| set | set | accesses by the forbidden functions, except the allowed ones |

- Which keys need a function's nested scopes named:
  - `scopes` and `allowed-callees` of an effect boundary name a function, class, or module and
    everything nested in it: `pkg.run:outer` covers `outer.<locals>.inner`, and `outer{,.*}` adds
    nothing.
  - `allowed-writers`, `allowed-readers`, `allowed-callers`, `forbidden-*`, `exempt-methods`, and
    the `invalidation` functions name the functions they match and no others. With `attribution =
    "call-graph"` (the default) a lambda, a comprehension, a generator expression, and a nested
    `def` that only runs inside the function are judged where the function is, so `allowed-*`
    naming `outer` covers them; a nested `def` that is handed on (returned, stored, passed) is a
    function of its own and needs its own entry, and `forbidden-*` names only the functions it
    matches. With `attribution = "lexical"` a lambda, comprehension, or generator expression
    still belongs to its function, and every nested `def` is judged by itself.
  - `targets` name the function, method, or class they match; a nested function is a target only
    where an entry names it.

  So `outer{,.*}` is needed in `allowed-*` and `forbidden-*` only for nested `def`s that
  `attribution` does not attribute to `outer`, and in `targets` for nested definitions.
- An access counts as one by a forbidden function when the function that is reported, a private
  helper it hands the access through, or the function making the access is forbidden (with
  `attribution = "lexical"`, the function holding the access).
- `allowed-*` and `exempt-methods` win wherever they apply on that path, as does the rule for
  functions that [only run inside allowed functions](#how-contracts-report).

### `effect-boundaries`

```toml
[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["*.render", "*.render_module_code", "pkg.domain.*"]
forbid = ["writes-targets", "blocking"]
write-targets = ["DataModel.*", "DataType.*"]
allowed-callees = ["pkg.logging.*"]
message = "render entry points must not mutate model state"

[[tool.heki.effect-boundaries]]
name = "decisions-read-final-facts"
scopes = ["pkg.model._decisions.*"]
forbid = ["reads-targets", "calls"]
read-targets = ["DataType.is_optional", "DataModel._additional_imports"]
call-targets = ["GenerationStore.*"]

[[tool.heki.effect-boundaries]]
name = "pass-sort-base-classes"
scopes = ["pkg.parser.passes.sort_base_classes"]
allow-writes = ["DataModel.base_classes"]
write-universe = ["DataModel.*", "DataModelFieldBase.*", "DataType.*"]

[[tool.heki.effect-boundaries]]
name = "portable-generation"
scopes = ["pkg.generate.*"]
forbid = ["reads-environment"]
import-probes = "report"
```

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The boundary's name in reports. |
| `scopes` | `[]` | Modules, classes, or functions whose functions must stay inside the boundary; nested functions are included. A module body is included only where a pattern names `<module>` (`pkg.gen:<module>`, below). |
| `forbid` | `[]` | What the scope functions must not reach (tokens below). |
| `write-targets` | `[]` | `Class.attr` targets of `writes-targets`, required with it. `Class.attr["key"]` names one entry, as in `write-owners`. |
| `read-targets` | `[]` | `Class.attr` targets of `reads-targets`, required with it: reads as `read-owners` sees them, also inside property getters. A container entry is a configuration error. An entry can also name an attribute of a module outside the project by its full name (`sys.platform`, `os.environ`, below). |
| `call-targets` | `[]` | Functions, methods, or classes of `calls`, required with it, matched as the `targets` of `call-owners` (also with an argument list). An entry can also name a function or class outside the project by its full name (`os.getenv`, `platform.*`, below). |
| `write-universe` | `[]` | `Class.attr` entries: a reached write of one that `allow-writes` does not name is reported, once per written `Class.attr`. |
| `allow-writes` | `[]` | `Class.attr` writes the boundary allows, exempt from `write-targets` too. Needs `write-universe`. |
| `allowed-callees` | `[]` | Functions, classes, modules, or ecosystem symbols whose effects are allowed: paths through them are not followed, and calls of them are no `calls` violation. |
| `fresh-objects` | `"allow"` | `"allow"` or `"report"`: whether a write that constructs the object it writes (as in `write-owners`) counts as a write of the boundary. |
| `references` | `"ignore"` | `"ignore"`, `"calls"`, `"calls-and-bases"`, or `"reads"`, as for `call-owners`: whether a reference handing a `call-targets` entry on (and a class statement naming one among its bases, and any other use of its value) counts for `calls`. |
| `import-probes` | `"allow"` | `"allow"` or `"report"`: whether `reads-environment` also forbids an import probe, an `import` inside a `try` with an `except` clause naming `ImportError` or `ModuleNotFoundError`. `"report"` without `reads-environment` is a configuration error. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the boundary also reports, under `EFF201`, what it cannot judge in its scopes' reach: a call heki resolves to no function, a function it does not model (`eval`, a body too large) or whose body only a stub declares, and a write of an attribute `write-targets` or `write-universe` names on an object of unknown class or under a name computed at run time. Each kind is one report per scope function, at the first one with its chain. |
| `message` | none | The help of every report of the boundary. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |

| `forbid` token | Forbids |
| -------------- | ------- |
| `blocking-net`, `blocking-db`, `blocking-fs`, `blocking-subprocess`, `blocking-sync`, `sleep` | that blocking effect |
| `blocking` | all six blocking effects |
| `mutates-params` | changing a parameter in place (for a method, `self` is a parameter) |
| `writes-targets` | writing a `write-targets` attribute |
| `reads-targets` | reading a `read-targets` attribute |
| `calls` | calling a `call-targets` function or class |
| `reads-environment` | reading the environment the program runs in: calling or reading what the ecosystem catalog tags `environment` (below), and import probes with `import-probes = "report"` |

Outside the project, `call-targets` and `read-targets` entries name symbols by their full dotted
name, starting with the top-level module: `os.getenv` matches a call of `os.getenv` (also
`from os import getenv`, `import os as o`), `platform.*` every function of `platform`, and
`sys.platform` a read of `sys.platform` (also `from sys import platform`, or `import sys as s`
and `s.platform`). A dotted suffix (`getenv`), a leading glob (`*.getenv`), or a module part (`os:getenv`) never
names a symbol outside the project, so an entry for project functions keeps its meaning.
`<external>._*` names the private members of every module and class outside the project, as for
the [owner contracts](#symbols-outside-the-project), whose rules for calls and reads these
entries follow. Only a call or a read
heki resolves counts: an attribute read by name (`getattr(sys, name)`) or on an object of unknown
origin is not reported.

`reads-environment` needs no targets. The ecosystem catalog tags these calls: `os.getenv`,
`os.getenvb`, the functions of `platform` that describe the machine or the interpreter
(`platform.system`, `platform.machine`, `platform.python_version`, ...),
`importlib.util.find_spec`, `importlib.import_module`, `importlib.metadata.version`,
`importlib.metadata.distribution`, `shutil.which`, and `sysconfig.get_config_var`; and these
reads: `os.environ`, `os.environb`, `os.name`, `sys.platform`, `sys.version`,
`sys.version_info`, `sys.hexversion`, `sys.implementation`, and `sys.flags`. An entry set
`excluded = true` (`[[tool.heki.ecosystem]]`, below) is never reported. A call on
`os.environ` (`os.environ.get("X")`) is a read of `os.environ`. One report per scope function
names the first read it reaches and counts every one.

`importlib.import_module(name, package)` and `importlib.util.find_spec(name, package)` read the
environment unless the module they import is established to be the project's own:

- every text `name` may hold names a module of the project (`"pkg.config"`, a module constant,
  either of two such texts) that the import finds in the project, or starts with a package of
  the project and a dot (`f"pkg.parser.{name}"`, `"pkg.parser." + name`, `"pkg.%s" % name`)
  under which it finds only modules of the project (below);
- a relative name (`".config"`, `f"..parser.{name}"`) is anchored at `package`: a text the code
  fixes, or the module's own `__package__` or `__name__`;
- the call is not inside a `try` with an `except` clause naming `ImportError` or
  `ModuleNotFoundError`: the code expects the import to fail.

Python finds the modules under a package on its `__path__`, the package's own directory unless
code changes it. heki follows what the modules of the project do to the `__path__` of a package,
its own in its `__init__.py` or another's through an import (`plugins.__path__`, `from plugins
import __path__`):

- adding directories after the package's own (`__path__.append(d)`, `__path__.extend(ds)`,
  `__path__ += ds`, `__path__ = __path__ + ds`, `__path__ = pkgutil.extend_path(__path__,
  __name__)`, also on `__import__("pkgutil")`, and `pkg_resources.declare_namespace`) leaves the
  package's own modules found first, but any other name under the package may be an installed
  module;
- any other change (`__path__ = [d]`, `__path__.insert(0, d)`, a slice store, `del __path__`), or
  a use heki does not follow (the list handed to a function other than a builtin that only reads
  it and `pkgutil.iter_modules`, `walk_packages`, or `extend_path`, returned, or stored
  elsewhere), may take any name under the package elsewhere, its own modules included. So may a
  package whose `__init__.py` heki cannot read.

Reading the path (iterating it, `__path__[0]`, `len(__path__)`, `pkgutil.iter_modules(__path__)`)
changes nothing. A module is found in the project when each package along its name leads the
import there first: heki takes the project's top-level packages to be found in the project, a
regular package of the project is found before the directories added after its parent's own,
and a directory without `__init__.py` is a namespace package with a portion in each directory of
its parent's path, in that order. A name under a package holds only modules of the project when
the packages along it lead to the project's directories alone and no package under it changes
its path: `f"pkg.{name}"` reads the environment once a package anywhere under `pkg` changes its
`__path__`, while a change in another package does not affect it.

A name heki cannot tell (a parameter, `"pkg" + name`, a namespace package without
`__init__.py`) stays a read of the environment, and so does an import of a module the project
does not have. A `call-targets` entry naming `importlib.import_module` matches every call of it.

- Each scope function is checked through any number of calls.
- A module body, what runs when the module is imported, is checked only where a scope names
  `<module>` itself: `pkg.gen:<module>` (the body of `pkg.gen`), `pkg.*:<module>` (of every
  module under `pkg`), `pkg.gen.<module>`, or `<module>` (of every module). `pkg.*` and
  `pkg.gen:*` keep naming the functions only. The body is checked as a scope function is (its
  module-level reads, calls, import probes, and writes), and the bodies of its classes, which
  run with it, count for what they take from outside the project (environment reads, import
  probes, and the external `call-targets` and `read-targets`), not for their other effects.
  Reports name it `<module>`.
- A violation inside another scope function is reported there only, once.
- One report per scope function and forbidden kind (or target) stands for every distinct site of
  it that the function reaches.

### `readonly-returns`

```toml
[[tool.heki.readonly-returns]]
name = "schema-graph"
producers = ["pkg.parser.SchemaGraphBuilder.schema"]
deep = true
```

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name. |
| `producers` | `[]` | Functions, methods, external symbols, or attributes (`Class.attr`) whose result every caller shares. |
| `deep` | `false` | Protect the values below the result too: its attributes, items, and elements. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |

- Functions decorated with `@lru_cache` / `@cache` are protected without configuration, also
  through an import alias, but not a `cache` decorator the module defines itself.
- Every function that returns a protected value, or a part of it, on some path is protected too:
  wrappers and property getters need no entry of their own.

### `invalidation`

```toml
[[tool.heki.invalidation]]
name = "graph-index"
targets = ["DataType.data_types", "DataModel.fields"]
invalidators = ["GenerationStore.note_changed_models", "GenerationStore.refresh_now"]
exempt-writers = ["pkg.parser.generation.*"]
message = "tell the store which models changed"
```

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | `Class.attr` attributes whose writes must be announced. |
| `invalidators` | `[]` | Functions or methods whose call announces the change. |
| `exempt-writers` | `[]` | Functions whose writes need no announcement of their own: the owner's operations, which announce or batch the change themselves, and construction methods. |
| `message` | none | The help of every report of the contract. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |

An invalidator call counts when it runs on every path from the write to the function's normal
return:

- directly, or in each branch;
- behind a boolean flag the path sets after the write;
- through a helper that calls one on every path of its own.

What needs no announcement:

- a path that ends by raising: only normal returns count;
- writes to objects that did not exist before the call: an initializer's own `self` (not the
  objects reached through its attributes), and objects the function constructs or copies;
- writes to objects the function gets from a call;
- a function whose every caller announces the change after calling it.

A write is followed through local aliases (also in the scopes nested in the function),
container methods, `setattr` / `__dict__`, and helpers. A helper changing an argument it does
not type writes the field or instance its caller binds to it. At most 64 `invalidation`
contracts can be declared.

### `import-boundaries`

```toml
[[tool.heki.import-boundaries]]
name = "facts-leaf"
importers = ["pkg.facts"]
allowed-imports = ["<stdlib>", "pkg._records"]

[[tool.heki.import-boundaries]]
name = "shared-model-backends"
importers = ["pkg.model.shared"]
forbidden-imports = ["pkg.model.backend", "pkg.model.backend.*"]

[[tool.heki.import-boundaries]]
name = "no-root-import"
importers = ["pkg.*"]
forbidden-imports = ["pkg"]
```

Which modules the modules of `importers` may import, checked by
[`IMP201`](rules/import-boundary-violation.md). Its names are module patterns: a whole module name
or a glob whose `*` spans dots, never a suffix (`pkg.*` names `pkg.facts` and
`pkg.model.shared`, not `pkg`), or one of the groups `<stdlib>` (the standard library),
`<project>` (the project's modules), and `<third-party>` (every other module).

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `importers` | every module | The modules judged. |
| `exempt-importers` | `[]` | Modules of `importers` that are not judged. |
| `allowed-imports` | `[]` | The modules the importers may import: every other import is reported, but those of `relative-imports`. `"pkg.db:Session"` allows one name of a module: only `from pkg.db import Session` imports it (a glob may stand in either part), and another name of `pkg.db` is still reported. |
| `forbidden-imports` | `[]` | When set, only imports of these modules are reported, less the allowed ones, as in [Allowed and forbidden functions](#allowed-and-forbidden-functions). `"pkg.parser:_*"` forbids the names of a module that match: `from pkg.parser.base import _helper` is reported through `"pkg.parser.*:_*"`, and `from pkg.parser import jsonschema` (a submodule) and `from pkg.parser.base import Parser` are not (a glob may stand in either part). A contract setting none of `allowed-imports`, `forbidden-imports`, and `relative-imports` allows no imports. |
| `relative-imports` | `[]` | The modules the importers may import only through a relative import ([Relative imports only](#relative-imports-only)). |
| `ignored-imports` | `[]` | `"importer -> imported"` pairs of module patterns whose imports the contract accepts (`"pkg.cli -> pkg.db"`; `"pkg.cli -> pkg.db:Session"` accepts one name of the module, as in `allowed-imports`). With `reach`, an import accepted or allowed by a name is not followed. |
| `type-checking` | `"ignore"` | `"ignore"` or `"report"`: whether the imports under `if TYPE_CHECKING:`, which never run, count. |
| `reach` | `"statement"` | `"statement"`: each import of an importer is judged. `"import-time"`: every module loading an importer loads while it loads. `"all"`: every module a chain of imports that run leads to. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the imports whose module heki cannot name are reported under [`IMP202`](rules/unresolved-guarded-import.md): a name the code does not fix, a call that may be `importlib.import_module` or `__import__` (a method of that name on an object of unknown class, a name bound to one on some path), and a relative import that climbs past the top-level package. |
| `message` | none | The help of every report of the contract. |
| `fail` | `false` | `true`: the reports of this contract make `heki check` (and `--watch`) exit with code 1, whatever their tier and severity; see [Failing on a contract](#failing-on-a-contract). |

- An import counts wherever it runs: in the module body, a class body, a function, or lazily,
  with or without an `ImportError` handler around it. `importlib.import_module` and
  `__import__` with a constant name count as imports of that module.
- `import a.b.c` imports `a.b.c`, not `a` or `a.b`, which it loads first. `from a.b import c`
  imports the submodule `a.b.c` when the package has one and binds no `c` before importing it,
  and `a.b` otherwise. An import of a module the project lacks below one of its packages fails,
  and is no import; `from a.b import c` of a name `a.b` binds nowhere still imports `a.b`, and
  may import a module `a.b.c` outside the project's files (a compiled extension module). When
  the package binds `c` to an object of its submodule after importing it (`from .c import c`),
  `from a.b import c` imports `a.b`, and also `a.b.c` while the package has not run that binding
  yet.
- One report per import and module. A report's target, which a
  [baseline](adoption.md#ratchet-baselines) counts it by, is the module imported.
- With `reach`, each module reached is reported once per importer, at the import of the importer
  that starts a shortest chain to it, with the other steps as related locations. A step through
  a parent package counts: loading `pkg.models.base` runs `pkg/models/__init__.py` first. The
  packages holding the importer load before it and are never reached. The walks of one contract
  share a budget proportional to the import graph, in the order of the modules; past it, the
  importers left get no report.
- Definitions are not import targets: who may use a class or a function is a
  [`call-owners`](#call-owners) or [`read-owners`](#read-owners) contract.

Outside a package, only its public entry: every module but the package's own may import
`app.billing`, and none of the modules under it.

```toml
[[tool.heki.import-boundaries]]
name = "billing-entry"
exempt-importers = ["app.billing", "app.billing.*"]
forbidden-imports = ["app.billing.*"]
message = "import `app.billing` and use what it exports"
```

`from app.billing import Invoice` imports the package, while `from app.billing import invoice`,
`import app.billing.invoice`, and `from app.billing.invoice import Invoice` import the
submodule.

A start-up contract: loading `pkg.cli` must not load `torch`, however many modules lie between
them.

```toml
[[tool.heki.import-boundaries]]
name = "startup"
importers = ["pkg.cli"]
forbidden-imports = ["torch", "torch.*"]
reach = "import-time"
```

#### Relative imports only

A package copied into other packages under another name works only while its modules import
each other relatively: `from pkg._runtime import b` names a package the copy does not have.

```toml
[[tool.heki.import-boundaries]]
name = "runtime-leaf"
importers = ["pkg._runtime", "pkg._runtime.*"]
allowed-imports = ["<stdlib>"]
relative-imports = ["pkg._runtime", "pkg._runtime.*"]
```

```python
# pkg/_runtime/a.py
from . import b  # allowed
from pkg._runtime import c  # IMP201: allowed only through a relative import
```

- An import names a module relatively when it gives the module's place from the importer:
  `from . import b`, `from .b import f`, `from .. import c`, and
  `importlib.import_module(".b", __package__)` (or `__name__`). By its full name are
  `import pkg._runtime.b`, `from pkg._runtime import b`,
  `importlib.import_module("pkg._runtime.b")`, `__import__("pkg._runtime.b")`, and a relative
  name anchored at a package the code spells, `importlib.import_module(".b", "pkg._runtime")`.
  `__import__` with a `level` other than `0` names a module heki cannot tell, which
  [`IMP202`](rules/unresolved-guarded-import.md) reports with `unresolved = "report"`.
  `heki inspect --imports` shows how each import names its modules (`spelling`).
- A module the key matches needs no other entry. Imported relatively, it is allowed, also when
  `forbidden-imports` names it; imported by its full name, it is reported, also when
  `allowed-imports` names it. An `ignored-imports` pair accepts the import either way. A by-name
  import given several names judges each module by the names that give it: one a relative name
  and a full name both give counts as named by its full name.
- The key takes module names and globs, not groups: a relative import reaches only the modules of
  the importer's own top-level package. An importer outside the package of a module the key
  matches cannot name it relatively, so each of its imports of that module is reported.
- The imports judged are the ones the contract counts, as for the other keys: wherever they run,
  and under `TYPE_CHECKING` only with `type-checking = "report"`. With `reach`, how an import
  names its module is judged only at the importer's own imports (with `"import-time"`, those that
  run while it loads): how the modules on a chain import each other is theirs, and a module of the
  key that a chain reaches is allowed.
- A report's target is the module imported, as for the other reports of the contract.

### Failing on a contract

`fail = true` in a contract table makes every report of that contract fail `heki check` (and
`--watch`): exit code 1, whatever the report's tier and severity. A contract without the key, or
with `fail = false`, keeps the default: its reports are warnings, and fail the run only as
`fail-on` says. Every kind of contract has the key.

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields"]
allowed-writers = ["Store.*"]
fail = true
```

- Other warnings and heuristic reports stay advisory: `fail = true` does not change `fail-on`,
  the rules' severities, or the tier of a report. The outputs (text, JSON, SARIF, GitHub
  annotations) are the same with and without it.
- A report that several contracts find stands for each of them, and fails the run when one of
  them has `fail = true`. A `MUT202` report is of the `readonly-returns` tables naming a producer
  the changed value may come from: all of them when the change reaches the producer's result
  itself, and only those with `deep = true` when it changes a value below it.
- A report that a [baseline](adoption.md#ratchet-baselines) records, or that an inline
  [suppression](#inline-suppressions) hides, does not fail the run. With `--touched`, the reports
  the run shows decide; the note about the ones it does not show says how many would fail it.
- The closing note of a text run, which counts the reports that do not fail the run, leaves these
  reports out.
- `fail = true` in a [`<kind>-defaults` table](#defaults-for-the-tables-of-a-kind) applies to
  every table of the kind that does not set `fail` itself.
- `fail-on = "warning"` is still there for a project that wants every warning and heuristic
  report to fail the run.

### How contracts report

These hold for `write-owners`, `read-owners`, and `call-owners`, and the reporting at callers
holds for `invalidation` too:

- **A class's own initialization is allowed.** An initializer (`__init__`, `__post_init__`,
  `__new__`, `model_post_init`, `__setstate__`) of a target class or a subclass may access its own
  `self`: directly, through `super().__init__()`, or through helpers it passes `self` to.
- **A nested scope uses the parameters of the functions enclosing it.** A comprehension,
  generator expression, lambda, or nested `def` reading a parameter of an enclosing function
  (`[self._fmt(x) for x in xs]`) reaches that parameter's object, as a helper the function hands
  it does: a call of the scope by that function, or by another scope nested in it, hands the
  object over. So a comprehension in `__init__` writing `self.imports` is part of the class's own
  initialization. The variable is another object when the function or a nested scope rebinds it
  (`self = other`, `nonlocal self`). A closure called away from the function (stored, then
  called later elsewhere) still reads the same object, but heki does not tie it to the
  parameter: no call of the function hands it over.
- **A function that only runs inside allowed functions is allowed.** Every path of project
  callers above it starts in an allowed function, an exempt method, or such an initialization.
- **A private helper reports at its callers.** An access in a private helper (`_name`) with
  project callers is reported at the call sites of its first non-private callers. A private
  function called only by its own recursion reports its accesses itself.
- **`attribution = "lexical"` judges the code where it is written.** The two rules above do not
  apply: an access is judged in the function whose code holds it, whoever calls that function.
  A lambda, comprehension, or generator expression is part of the function it is written in, a
  class body's code (a class attribute's value, a decorator, a default argument) part of the
  function running the class statement (`<module>` for a class at module level), and a nested
  `def` a function of its own; the parameters of the function a lambda, comprehension, or generator
  expression reads are the function's, wherever it runs. For `write-owners`, a call handing a
  typed object to a helper that changes it without knowing its class is the caller's write.
  With `unresolved = "report"`, the callers binding a parameter only tell its object's class,
  allowed callers included. A class's own initialization stays allowed, a helper it passes
  `self` to included when every path of project callers above the helper starts in such an
  initialization; so do `exempt-methods`, `fresh-objects`, and the targets of a call contract
  calling each other. Effect boundaries have no such key: they judge what their scope functions
  reach through any number of calls.
- **`own-receiver = "allow"` allows the accesses through a method's own receiver.** An access is
  allowed when the function it is judged in (the function reported, or with `attribution =
  "lexical"` the function holding the access) is a method that is no static method, and the
  object the member is looked up on may be its first parameter: `self._cache[k] = v`,
  `self._seen`, `cls._kind()`, `super()._label()`, `self._norm(x)` of a static method,
  `Resolver._label(self)`. A member the class inherits counts like its own. Another parameter, an
  attribute of the receiver, a static method called through its class, and the parameter of a
  static method or of a function stay reported. By the call graph, a private helper the method
  hands its `self` (`_count(self)`) is judged at the method, and is allowed with it, and so are
  a comprehension, generator expression, lambda, or private nested `def` the method runs that
  uses its `self`; a public nested `def` reports itself, as a public helper does. With
  `attribution = "lexical"`, a lambda, comprehension, or generator expression using the method's
  `self` is allowed wherever it runs, and a nested `def` is judged by itself, as any helper is.
- **`allowed-protocols` allows the accesses through the protocols it names.** A read, call, or
  write through a value typed with a project protocol a pattern names (or a subprotocol of one)
  that declares the member, itself or through its protocol bases, is allowed wherever it is
  judged, and so is a reference that `references` counts (`return h._helper`). So is one through
  a class object typed `type[P]` of such a protocol (a parameter, a local, or an attribute
  annotated so: `cls._items_constraints(obj)` with `cls: type[KeywordsHost]`), and one through a
  protocol whose implementations heki cannot list, though heki cannot tie either to a class
  (`MUT205`, `MUT206`, `MUT207`): `allowed-protocols = ["*Host"]` allows `parser._flag` with `parser: RenderHost` and
  `class RenderHost(RuntimeFlags, Protocol)` when `RuntimeFlags` declares `_flag`, while the same
  read through `flags: RuntimeFlags` stays reported. With `protocol-subclasses = false`, a
  pattern names the protocols it matches only, not their subprotocols: with
  `class RuntimeExpressionFlags(GraphBuilderHost, Protocol)`, `allowed-protocols = ["*Host"]`
  allows the accesses through `GraphBuilderHost`, and those through `RuntimeExpressionFlags`
  stay judged, also of the members it inherits from `GraphBuilderHost`. A write through the
  protocol is allowed for the protocol's member and for each implementation's attribute alike.
  Accesses through a
  protocol that does not declare the member, through an implementation's class, through a value
  of unknown class that no annotation types with such a protocol (`unresolved = "report"`), and a
  protocol-typed value handed to a helper that changes it without typing its parameter stay
  reported. By the call graph, a private method of an implementation that a call
  through the protocol runs still hands its writes of its own `self` to the caller.
- **The help names the allowed code that does it today.** After the contract's `message`, the
  help of a report names up to three public functions and methods whose own body does what the
  report shows from a place the contract allows (an `allowed-*` pattern names them, or the
  `forbidden-*` patterns leave them out; for `invalidation`, the functions announcing every
  write of the target on every path and the `exempt-writers`), by fully qualified name:
  ``add entries through `Store`; allowed writers of `Registry.entries`: `pkg.store.Store.add` ``.
  Private functions (`_name`), special methods (`__init__`), and the exempt methods are not named.
  JSON output gives them with their files as `allowed_examples` (`fqn`, `path`).

### Entries that match nothing

A contract entry that names nothing guards nothing: renaming or removing what it named leaves the
contract silent. `heki check` warns about each such entry, once per kind, key, and text, naming
the contracts that hold it and the existing names one part away (the same holder and a like
last part, or the same last part and a like holder), and
[`heki recommend`](recommend.md) lists them:

```console
$ heki check
warning: `targets` entry `pkg.model.Field._gone_cache` of write-owners `field-caches` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files; `pkg.model.Field._type_cache` exists
```

- Judged, in the contracts whose rules run: the targets, `allowed-*`, `forbidden-*`,
  `exempt-methods`, `allowed-protocols`, and `forbid-cached-property-on` of `write-owners`,
  `read-owners`, and `call-owners`; the `scopes`, `write-targets`, `write-universe`, and
  `allow-writes` of `effect-boundaries`; and the `targets`, `invalidators`, and
  `exempt-writers` of `invalidation`. The `read-targets`, `call-targets`, and
  `allowed-callees` of an effect boundary and the `producers` of `readonly-returns` may name
  symbols outside the project, and are not judged.
- A `call-owners` or `read-owners` target whose first name is no module of the project may name
  code outside it. It is matched when the analyzed files call or read a symbol outside the
  project it names, and it is not judged when it is an `<external>` entry or its first name is a
  module outside the project (one of the standard library, or one a module of the analyzed files
  imports at module level): it is a rule about any use of that code, now or later, and heki
  cannot list what such a module has. Any other entry is judged as above, so a misspelled class
  (`Feild.removed`) is still reported, and so is an entry naming project code a run over some
  files leaves out.
- Each entry is matched as its rule matches it, against the definitions of the analyzed files:
  a function pattern names a function, method, class, or module body; a call target a function,
  method, or class; an exempt method a method's name; a scope a module, class, or function; a
  class pattern a class.
- A `Class.attr` entry needs a project class, and an attribute the class family has: a name its
  class body binds (a method, an annotation, an assignment), one its methods or its module set
  on its instances, one of a project base, an attribute or a constant `__dict__` key the code
  writes on an object typed with the class or a subclass (a `__dict__` key only read does not
  exist, except for a `read-owners` target, which guards the reads), or one written on an object heki cannot type. A key a loop takes from a tuple, list, or set of string constants
  (`for key in ("_a", "_b")`), or from a module's tuple of them (`for key in RECORDS`), names
  each of them, a list or set only while no code changes it, and so does the key of each pair a
  generator of them gives `update` (`ns.update((key, v) for key in RECORDS)`). A module
  `__getattr__` caching what it serves (`globals()[name] = value`) leaves its module's string
  constants bound on every path known, since Python calls it only for a name the module lacks,
  unless a decorator wraps it or another module loads it (`keys.__getattr__("KEY")`).
  A class whose module stores attributes heki
  cannot name (`setattr(self, name, v)`), or that the code gives a name computed at run time,
  may have any attribute: an entry naming none of its known ones is not reported, except with
  `unmatched-contract-entries = "error"`, as one that names nothing "unless it is one the code
  names at run time". A key taken from the namespace of an instance of the class or a
  subclass (`for key, value in other.__dict__.items()`, `.keys()`, iterating it or
  `vars(other)`, also through `list`, `tuple`, `sorted`, or a copy, and
  `obj.__dict__.update(other.__dict__)`) names an attribute the instance has already, as a
  `__deepcopy__` copying it does, and a deletion adds none: neither gives the class a name. So
  does a key taken from the fields the class or a subclass declares, where a library keeps them
  (heki's catalog): a pydantic model's `model_fields`, `__pydantic_fields__`, or `__fields__`
  read on the class (`type(obj)`, `obj.__class__`, `cls`) or an instance, with their keys and
  pairs as for a namespace; a dataclass's `__dataclass_fields__` and the `name` of each of
  `dataclasses.fields(obj)`; an attrs class's `__attrs_attrs__`, `attrs.fields(cls)`, and
  `attrs.fields_dict(cls)`.
  An attribute that only a base outside the project defines matches when the code
  uses it on the class. An entry target (`Class.attr["key"]`) is judged by its attribute.
- An entry whose last part is a glob is a rule about every name it may match, later ones
  included: it needs only what holds that part (`pkg.store` of `pkg.store:*`, the class of
  `Field._*`, the function of `run.*`). A glob without a literal holder (`*_helper`, `__*__`) is
  not judged.
- The names offered are found by comparing an entry with the names that share its holder or its
  last part. A run makes at most 64 such comparisons per function and class of the project (at
  least 4,096) for all its entries, so of thousands of entries that name nothing in a large
  module only the first get names. Every entry is still reported.
- The warnings do not fail the run; with `unmatched-contract-entries = "error"` they are errors,
  which fail it, and `"ignore"` turns them off. A suppression comment or a baseline does not
  hide them.
- Without the key, only a run over the whole project judges: one whose paths leave out Python
  files below the project root (`heki check pkg/stores`) would find unmatched the entries naming
  code in the others. A file given is checked in its project, so its run judges. With the key
  set, every run judges the files it analyzes.
- `--watch` and the language server do not report them.

## Imports: `[tool.heki.imports]`

Which imports the import rules count, and which distributions provide the modules they import.

| Key | Default | Meaning |
| --- | ------- | ------- |
| `cycle-edges` | `["body"]` | The timing classes of the imports [`IMP101`](rules/import-cycle.md) follows when it looks for modules importing each other: `"body"`, `"lazy"`, `"function"`, and `"type-checking"`. |
| `module-distributions` | `{}` | The distributions that may provide a module outside the project, by the module: [`IMP301`](rules/development-dependency-at-import-time.md) and [`IMP302`](rules/optional-dependency-at-import-time.md) take them for the module and the modules below it, instead of the catalog's and the distributions named like the module. |

```toml
[tool.heki.lint]
extend-select = ["IMP101"]

[tool.heki.imports]
cycle-edges = ["body", "lazy"]   # also the lazy imports the module body does not read
```

- `"body"`: an import that runs while its module loads: in the module body or a class body the
  module body runs, on any branch or inside `try`. A constant by-name import there
  (`importlib.import_module("pkg.db")`) counts, and so does a lazy import (`lazy import`, or a
  name a literal `__lazy_modules__` lists) whose name the module body reads.
- `"lazy"`: a lazy import whose name the module body does not read; it runs at its first use.
- `"function"`: an import inside a function, which runs when the function does.
- `"type-checking"`: an import under `if TYPE_CHECKING:`, which never runs.
- An import whose class heki cannot tell (a lazy import whose reads it cannot place) counts for
  no class.

`IMP301` and `IMP302` compare the modules a package loads with the distributions its
`pyproject.toml` declares. A distribution provides the modules named like it (PEP 503 names:
`ruamel.yaml` and `ruamel_yaml` are both `ruamel-yaml`), and the catalog knows the common ones
named otherwise (`PyYAML` provides `yaml`, `Pillow` provides `PIL`). For any other,
`module-distributions` says it, and it also settles a module several declared distributions may
provide:

```toml
[tool.heki.imports]
module-distributions = { cv2 = ["opencv-python-headless"], "google.cloud.storage" = ["google-cloud-storage"] }
```

- Each key is a module name; it covers the module and every module below it, and the longest key
  naming a module decides it.
- Distribution names are compared as PEP 503 normalizes them. A module the map gives only
  distributions the project does not declare is not judged.

## Ecosystem knowledge: `[[tool.heki.ecosystem]]`

heki ships a catalog of blocking, async-safe, and offloading APIs of the standard library and
common packages (`requests`, `httpx`, `aiohttp`, `sqlalchemy`, `django`, `redis`, `pandas`, …).
Add your own or override an entry:

```toml
[[tool.heki.ecosystem]]
symbol = "mypkg.db.run_query"
effects = ["blocking_db", "db_query"]

[[tool.heki.ecosystem]]
symbol = "mypkg.workers.in_thread"
offloads-blocking-arg = 0     # calls its first argument on a thread
async-safe = true

[[tool.heki.ecosystem]]
symbol = "mypkg.logging.emit"
excluded = true               # never reported
```

| Key | Meaning |
| --- | ------- |
| `symbol` | Fully qualified function or method name (`package.module.Class.method`). |
| `effects` | Any of `blocking_net`, `blocking_db`, `blocking_subprocess`, `blocking_sync`, `sleep`, `blocking_fs`, `unpooled_client_alloc`, `db_query`, `quadratic_realloc`. |
| `async-safe` | Safe to call from `async def`: it never blocks the event loop, so the blocking effects in `effects` are ignored (`db_query` and `unpooled_client_alloc` still count in loops). |
| `offloads-blocking-arg` | Index of the callable argument this function runs on another thread (the receiver is not counted). |
| `excluded` | Never reported (logging and similar). |
| `copy-sanitizer` | Returns a copy of its first argument (or receiver), ending `MUT202` protection: `"deep"` copies everything below it too; `"shallow"` (or `true`) copies only the object itself, so the values below it stay protected. |
| `return-type` | The class of the objects the call returns (a factory: `mypkg.db.open` returning `sqlite3.Connection`), whose methods are then looked up in the catalog. `"Self"`: an object of the receiver's class (a builder method such as `with_options`, also on a project subclass of the library class). |
| `attribute-type` | The class of what reading the attribute `symbol` names (`package.module.Client.chat`) on an instance of a library class gives, whose members are then looked up in the catalog: `client.chat.completions.create(...)` reaches the catalog's `create`. It takes no other key. The read is typed only where nothing in the project may change it: no project subclass of the class binds the name or defines `__getattribute__`, and no base heki knows nothing of comes before the library class. |
| `sync-lock-or-tx` | A `with` over an instance of this class, or over the result of this call, holds a synchronous lock or a database transaction (`EFF106`); an instance of such a class is a thread lock for `EFF117`. |
| `non-reentrant-lock` | An instance of this class is a lock its holder cannot take again: acquiring it while the same thread or task holds it waits forever (`EFF109`). |
| `one-at-a-time` | Tasks may not be inside this method on one object at once (`EFF110`): `"always"` (a second task's call raises), `"when_unbound"` (only while the object holds no connection, as an `AsyncSession`), or `"unbinds"` (as `"when_unbound"`, and the object holds no connection after the call: `commit`). |
| `writes-in-order` | The call writes an argument to a file or a stream, or serializes it into text, in the order of its elements (`EFF112`): a mapping's keys and a sequence's items in their iteration order. `{ arg = 0, param = "data" }` names the argument (a position not counting the receiver, or a keyword; `{}`: every positional argument, as `print`), `sorted-by = "sort_keys"` the keyword argument that sorts mapping keys first, and `sorted-by-default = true` a call that sorts them unless told otherwise. |

- An override sets the keys it lists on the built-in entry for the same symbol. The keys it leaves
  out keep their built-in values; `effects = []` removes every effect.
- An entry may name a function or method of the project, by the module that defines it (the
  built-in catalog never describes the project's own functions).

For a project function:

| Key | Effect |
| --- | ------ |
| `effects` | added to what heki finds in its body |
| `excluded` | keeps every effect of the body from its callers |
| `async-safe` | keeps the blocking effects of the body from its callers |
| `return-type` | types its result, over its annotation |
| `offloads-blocking-arg` | the parameter the body offloads, counted as for a library method |
| `copy-sanitizer`, `sync-lock-or-tx`, `non-reentrant-lock`, `one-at-a-time`, `writes-in-order`, `attribute-type` | not read: library symbols only (the project's own declarations type its attributes) |

## Inline suppressions

```python
x = load()            # heki: ignore[EFF101]
y = load()            # heki: ignore[EFF101, async-blocking-call]
z = load()            # heki: ignore
w = load()            # noqa: EFF101
```

- A suppression applies to the line of the diagnostic.
- Codes and rule names are accepted.
- A comment without codes (`# heki: ignore`, `# noqa`) silences every rule on that line.
- A bracketed list that is not closed or names no rule (`# heki: ignore[`, `# heki: ignore[]`)
  silences nothing.
- To keep a rule away from whole files, use [`per-file-ignores`](#rule-selection-toolhekilint).

### Unused suppressions

```toml
[tool.heki.lint]
report-unused-suppressions = true
```

With `report-unused-suppressions = true`, `heki check`, `--watch`, and the language server report
each code of a suppression comment that suppressed no report in the run (`unused-suppression`), as
Ruff's `RUF100` and basedpyright's `reportUnnecessaryTypeIgnoreComment` do:

```console
$ heki check --output-format concise
a.py:6:12: unused-suppression No `EFF101` report on this line to suppress
```

- A code is judged only when its rule ran on the file. A rule that did not run reported nothing a
  suppression could match, so its codes are never reported: a heuristic rule left off (select it,
  or set `heuristic = true`, to have its suppressions judged), an opt-in rule not selected, a
  rule `--select` leaves out or `per-file-ignores` turns off in the file, and `MUT103` without
  its summary snapshot.
- A code that names no heki rule is another tool's and stays: `F401` in `# noqa: F401, EFF101`.
  A suppression without codes is never reported as unused: a bare `# heki: ignore` covers rules
  that did not run, and a bare `# noqa` belongs to Ruff as much as to heki.
- A run judges what it reported, under its configuration: with `relax-async-in-tests`, a
  suppression of `EFF101` in a test suppresses nothing. A run over a directory of a project misses
  the reports that come from the others (`heki check app` when the blocking call is in `lib/`),
  so judge, and fix, on the whole project: to look at some files, name them or pass them to
  `--touched`, which keeps the analysis whole, as the pre-commit hook does.
- `heki check --fix` (and `--diff`) removes the unused codes, and the language server offers the
  same edit as a quick fix. The other codes stay; a suppression left without codes goes, with
  the blanks before it, and so does its line when the comment is alone on it. Other text of the
  comment stays (`# type: ignore`). Without the setting, `--fix` leaves suppressions as they are.
- The reports are errors, so they fail `heki check`. A suppression comment does not suppress
  them: the setting turns them off. A baseline records them as it records other reports.
- With a [baseline](adoption.md#ratchet-baselines), `heki check` also reports an entry that
  matched no report of a rule that ran on its file, when the file was checked, and exits with
  code 1: `error: baseline entry a.py::handler EFF101 `time.sleep` matched no report
  (`--update-baseline` removes it)`. A heuristic report that the run withholds still matches its
  entry. Without the setting, the entry gets a note that it can shrink.

### Suppressions that name no rule

```toml
[tool.heki.lint]
require-suppression-codes = true
```

With `require-suppression-codes = true`, a `# heki: ignore` without codes is reported
(`suppression-without-code`, an error), as basedpyright's `reportIgnoreCommentWithoutRule` does.
Its help names the rules it suppressed in the run (`# heki: ignore[EFF101]`). It still suppresses
every rule on its line: the setting reports the comment and changes nothing else. A bare `# noqa`
is not reported, since Ruff reads it too (its `PGH004` reports it).
