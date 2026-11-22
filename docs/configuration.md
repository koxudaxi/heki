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
fail-on = "warning"                 # warnings fail heki check too (default: "error")

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

One run checks one project: its configuration applies to every file of the run, also to files
given from another project. `heki check a/one.py b/two.py` uses `a/`'s configuration for both, so
check projects with their own configurations in separate runs: `heki check a/`, then
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

- an unknown key in any of heki's tables (the message names it: ``unknown field `bogus` ``);
- a `[tool.heki]` table in a file that is not named `pyproject.toml`;
- a heki setting at the top level of a `pyproject.toml` (`[[ecosystem]]` instead of
  `[[tool.heki.ecosystem]]`).

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
| `exclude` | the Ruff defaults (`.git`, `.venv`, `build`, `dist`, `node_modules`, …) | Replaces the excluded paths: globs, matched against the project-relative path and each of its components, and covering everything below a matched directory. |
| `extend-exclude` | `[]` | Adds excluded paths. |
| `src` | the detected source roots | Source roots, relative to the project root: the module names of the files below each are relative to it (`src = ["lib"]` names `lib/pkg/net.py` `pkg.net`). See [Module names follow the source roots](#module-names-follow-the-source-roots). |
| `baseline` | none | Ratchet baseline file, relative to the project root, used when `--baseline` is not given: by `heki check`, `--watch`, and the language server. See [Ratchet baselines](adoption.md#ratchet-baselines). |

### The default target version

Without `target-version`, heki reads `requires-python` from the `pyproject.toml` that holds the
configuration, or from the one next to `heki.toml` or the `--config` file. It takes the oldest
supported version that every clause admits:

| `requires-python` | Target |
| ----------------- | ------ |
| `>=3.11` | `py311` |
| `>=3.11, !=3.11.*` | `py312` |
| `>=3.9, <3.11` (only older versions) | `py311` |
| `>=3.16` (only newer versions) | `py315` |
| `<4` (no lower bound, admits 3.12) | `py312` |
| missing, or not a version specifier | `py312` |

### Which files are checked

- Without paths, `heki check` checks every `.py` and `.pyi` file under the current directory.
- Discovery skips hidden files and directories, the paths `exclude` and `extend-exclude` match,
  and the entries of `.gitignore` (inside a Git repository), `.ignore`, and `.hekiignore` files.
- Files named on the command line are always checked.

## Rule selection: `[tool.heki.lint]`

| Key | Default | Meaning |
| --- | ------- | ------- |
| `select` | the exact rules and the contract rules | Rules to enable instead of the defaults. An empty list enables no rule before `extend-select`, as in Ruff. |
| `extend-select` | `[]` | Rules to add. |
| `ignore` | `[]` | Rules to disable. |
| `per-file-ignores` | `{}` | Glob → rules disabled in matching files (project-relative POSIX paths). As in Ruff, `*` also matches `/`: `scripts/*.py` covers `scripts/tools/worker.py` too. |
| `severity` | `"warning"` for the rules that report slower code (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `EFF111`, `EFF113`, `DATA101`, and the `OPT` rules) and for `MUT110`, which reports a bug that is rarely harmful; `"error"` for the others, which report bugs | Rule → `"error"` or `"warning"`. It changes the output and the SARIF level. By default only errors make `heki check` fail; `fail-on = "warning"` makes warnings fail it too. |
| `relax-async-in-tests` | `true` | Skip `EFF101`, `EFF111`, `EFF105`, and `EFF106` in test code as pytest finds it: `test_*` functions and the functions nested in them, `test_*.py` and `*_test.py` files, and files under `test/` or `tests/`. |
| `heuristic` | `false` | Whether the heuristic rules that are not opt-in run by default and for a prefix or `ALL`, as the exact rules do. `--heuristic` and `--no-heuristic` override it. |
| `fail-on` | `"error"` | The least severity whose reports make `heki check` (and `--watch`) exit with code 1: `"error"`, or `"warning"` to fail on warnings too. Warnings are reported either way, and the language server shows them. `--fail-on` overrides it. |

- A rule is named by its code (`EFF101`), its name (`async-blocking-call`), a code prefix
  (`EFF`, `MUT2`), or `ALL`. An unknown rule is a configuration error.
- Every rule is exact or heuristic ([Rule Precision](precision.md)). The defaults, a prefix, and
  `ALL` enable the exact rules and the contract rules; with `heuristic = true` they also enable
  the heuristic rules that are not opt-in, as Ruff's `preview` does. Its code or name enables a
  heuristic rule either way.
- An exact rule's report that did not establish every premise of its claim (an `OPT` report
  whose rewrite is not proven) is heuristic, and is shown where a heuristic rule would run: with
  `heuristic = true`, or when its code or name is selected.
- A run of `full` or `concise` output that leaves heuristic rules off, or heuristic reports out,
  says how many.
- A prefix or `ALL` never enables the opt-in rules (`EFF105`, `EFF106`, `EFF107`, `EFF109`,
  `EFF110`, `EFF112`, `EFF114`, `MUT101`, `MUT103`, `MUT106`, `MUT107`, `MUT108`, `MUT111`, `MUT112`,
  `MUT113`, `MUT115`): they need their exact code or name. In `ignore`, a prefix covers every
  rule it matches.
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
| [`invalidation`](#invalidation) | writes that must be announced | `MUT109` | heuristic |

- These rules report nothing until a contract exists. The results of `functools.lru_cache` /
  `functools.cache` functions are checked without one by `MUT114` (`cached-result-mutation`).
- Declaring a contract selects its rule, whatever its certainty.
- Every name in a contract is a dotted pattern. It matches the full name or any dotted suffix
  (`DataModel.fields` matches `pkg.model.DataModel.fields`, not `pkg.BigModel.fields`), and it
  may be a glob (`*`, `?`, `[...]`, `{a,b}`).
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
  names no project module is no error: the name matches nothing.
- A report of a glob target names the member it matched (`reads
  `pkg.types.DataType._flag``); one report standing for several members lists the others.
- An access matching several targets of one contract (a target on a class and one on its
  subclass, a protocol's member and its implementation's attribute) is one report of the
  contract: it names the target configured first among them, and lists the others (`(also
  `A.items`)`).
- A `Class.attr` target covers the subclasses of the class too.
- A contract heki cannot use is a configuration error: a target that is not `Class.attr`, an
  invalid glob, a name with more than one colon or an empty module or qualified name, call
  targets whose globs are too many to match together (tens of thousands of complex ones), an
  argument list heki cannot read, an invalid glob in `allowed-protocols`, or an unknown value of
  `forbid`, `fresh-objects`, `unresolved`, `references`, `attribution`, `own-receiver`, or
  `unlisted-dict-keys`.
- With `unresolved = "report"`, a contract also reports the accesses it cannot tie to a class,
  each under its own rule (`MUT205` writes, `MUT206` reads, `MUT207` calls): an attribute or
  method named like a target, accessed on an object of unknown class (an unannotated parameter,
  a loop variable, an untyped call result, `Any`, a protocol whose implementations heki cannot
  list). A baseline counts them apart from the violations the typed rules prove.
- An attribute named through a string (`setattr`, `getattr`, `__dict__`, `vars`) is followed
  when the string is a constant: see [Dynamic names](scope.md#dynamic-names).

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
| `fresh-objects` | `"allow"` | `"allow"` or `"report"`: whether a write that constructs the object it writes is reported. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the writes the contract cannot tie to a class are reported under [`MUT205`](rules/unresolved-guarded-write.md). |
| `attribution` | `"call-graph"` | `"call-graph"` or `"lexical"`: which function a write is judged in. See [How contracts report](#how-contracts-report). |
| `own-receiver` | `"report"` | `"report"` or `"allow"`: whether a write through the receiver of the method it is judged in (`self`, `cls`, `super()`) is allowed. See [How contracts report](#how-contracts-report). |
| `allowed-protocols` | `[]` | Project protocols (class patterns, their subclasses included): a write through a value typed with one that declares the attribute, itself or through its protocol bases, is allowed. See [How contracts report](#how-contracts-report). |
| `protocol-subclasses` | `true` | `false`: an `allowed-protocols` pattern names the protocols it matches, not their subprotocols (a protocol deriving from an allowed one stays judged). |
| `unlisted-dict-keys` | `"ignore"` | `"ignore"` or `"report"`: whether a `__dict__` key of a target's class that no `write-owners` target names is reported when it is written. |

A container entry (`Class.attr["key"]`):

- matches the writes of that constant key (a literal or a string constant), not a write of the
  whole container or with a computed key;
- is reported once, under the entry, when its container is a target too.

A write into the entries of a target's container (`model.meta["kw"][name] = v`) is a write of
the target.

With `fresh-objects = "allow"`, a write is no violation when it constructs the object it writes:

- the object was allocated in the same call chain: by calling its class, by `object.__new__`
  (`object.__new__(Node)`), by a copy, or by a project function returning a new object;
- nothing can hold it yet: no store, `yield`, closure, or call that may keep it ran before the
  write.

A shallow copy's containers are the original's: only rebinding its attributes constructs it.

With `unlisted-dict-keys = "report"`, `MUT201` also reports a constant key written through the
namespace of an object of a class a target covers (`field.__dict__[KEY] = v`,
`field.__dict__.setdefault("_cache", v)`, `del vars(field)["k"]`) when no target covering that
class names the key, in this contract or in any other `write-owners` contract, so a new cache key
needs a target first. A target names the keys its attribute pattern matches (`Field._*` names
`_cache`), and an entry target names its container. A key another contract names is that
contract's to guard: its writes are judged by that contract's allowance alone. The contract's
allowance applies to an unlisted key as to a target write. A key computed at run time has no name
to check (`MUT205`, with `unresolved = "report"`), and neither has a write in a helper that does
not type the object. A key every function may write is listed by a contract that allows every
writer:

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
  of unknown class, where it may be any target.

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
and writes through the protocols it names (see [How contracts report](#how-contracts-report)).

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | `Class.attr` attributes. A container entry (`Class.attr["key"]`) is a write target only, and a configuration error here. |
| `allowed-readers` | `[]` | Functions allowed to read the targets. |
| `forbidden-readers` | `[]` | When set, only reads by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to read the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the reads of a target's attribute name on an object of unknown class are reported under [`MUT206`](rules/unresolved-guarded-read.md). |
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
| `targets` | `[]` | Functions, methods, or classes. A `Class.method` target covers the method's overrides in subclasses; a class target means constructing the class or a subclass. A target may end in an argument list (`DataType(is_dict=True)`, below). |
| `subclasses` | `true` | `false`: a class target means constructing exactly the class, not a subclass (an exception class whose subclasses anyone may raise). `Class.method` targets keep covering overrides. |
| `allowed-callers` | `[]` | Functions allowed to call the targets. |
| `forbidden-callers` | `[]` | When set, only calls by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to call the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the calls of a method named like a target (its last name) on an object of unknown class are reported under [`MUT207`](rules/unresolved-guarded-call.md). |
| `references` | `"ignore"` | `"ignore"`, `"calls"`, `"calls-and-bases"`, or `"reads"`: whether a reference handing a target on to be called later counts as a call of it (`[decide]`, `return decide`, `partial(decide)`, `field(default_factory=Rule)`, `obj.callback = self.decide`), with `"calls-and-bases"` a class statement naming a class target among its bases too (`class Derived(Base)`), and with `"reads"` every other use of a target's value as well (`Keys.of(...)`, `Code.OK`, `isinstance(x, Keys)`, `case Keys():`, `tuple[Keys, int]`). |
| `attribution` | `"call-graph"` | `"call-graph"` or `"lexical"`, as in `write-owners`. |
| `own-receiver` | `"report"` | `"report"` or `"allow"`, as in `write-owners`. |
| `allowed-protocols` | `[]` | As in `write-owners`, for a read or a call through the protocol. |
| `protocol-subclasses` | `true` | As in `write-owners`. |

- A call is reported only when every function it may call (each candidate in the class
  hierarchy) is a target: `text.replace(...)` on a `str` is never a call of
  `DataType.replace_reference`.
- Bound-method aliases (`replace = dt.replace_reference`) and `getattr(dt, "replace_reference")`
  with a constant name are followed.
- The targets of a contract may call each other: a call of the guarded operation is checked where
  it is made, so what the operation does inside is part of it.
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
  a dict display with string constant keys (literals, or string constants as above), which nothing
  changes (a name holding it is only unpacked with `**`, bound to another name, tested, or
  returned), passes its entries as keywords: `DataType(**{"is_dict": True})`,
  `flags = {"is_dict": True}; DataType(**flags)`. A call that raises `TypeError` instead of running
  the function passes nothing: two keywords of one name (written and in such a display, or in two
  displays), a keyword binding a parameter a position or the receiver binds too, a keyword naming no
  parameter of a function without `**kwargs`, a position (the receiver included) past the parameters
  of a function without `*args`, or a parameter without a default left unbound where no `*`
  unpacking or `**` unpacking of another mapping may bind it. Any other argument (a variable, a
  call, `*` unpacking, `**` unpacking of another mapping) is not established, and the call is not
  reported. A reference passes no arguments, so it never matches. A target with an argument list
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
| `message` | none | The help of every report of the boundary. |

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
names a symbol outside the project, so an entry for project functions keeps its meaning. Only a call or a read
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
  (`self = other`, `nonlocal self`), and for a closure called away from the function (stored,
  then called later elsewhere).
- **A function that only runs inside allowed functions is allowed.** Every path of project
  callers above it starts in an allowed function, an exempt method, or such an initialization.
- **A private helper reports at its callers.** An access in a private helper (`_name`) with
  project callers is reported at the call sites of its first non-private callers. A private
  function called only by its own recursion reports its accesses itself.
- **`attribution = "lexical"` judges the code where it is written.** The two rules above do not
  apply: an access is judged in the function whose code holds it, whoever calls that function.
  A lambda, comprehension, or generator expression is part of the function it is written in, a
  class body's code part of the function running the class statement, and a nested `def` a
  function of its own; the parameters of the function a lambda, comprehension, or generator
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
  judged, and so is a reference that `references` counts (`return h._helper`): `allowed-protocols = ["*Host"]` allows `parser._flag` with `parser: RenderHost` and
  `class RenderHost(RuntimeFlags, Protocol)` when `RuntimeFlags` declares `_flag`, while the same
  read through `flags: RuntimeFlags` stays reported. With `protocol-subclasses = false`, a
  pattern names the protocols it matches only, not their subprotocols: with
  `class RuntimeExpressionFlags(GraphBuilderHost, Protocol)`, `allowed-protocols = ["*Host"]`
  allows the accesses through `GraphBuilderHost`, and those through `RuntimeExpressionFlags`
  stay judged, also of the members it inherits from `GraphBuilderHost`. A write through the
  protocol is allowed for the protocol's member and for each implementation's attribute alike.
  Accesses through a
  protocol that does not declare the member, through an implementation's class, through a value
  of unknown class (`unresolved = "report"`, also a protocol whose implementations heki cannot
  list), and a protocol-typed value handed to a helper that changes it without typing its
  parameter stay reported. By the call graph, a private method of an implementation that a call
  through the protocol runs still hands its writes of its own `self` to the caller.
- **One report stands for every site behind it.** A report at a call stands for every access
  behind the call, and an effect boundary's report for every site its scope function reaches.
  JSON output gives their number as `sites`, and a
  [Ratchet baseline](adoption.md#ratchet-baselines) counts each of them.

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
| `return-type` | The class of the objects the call returns (a factory: `mypkg.db.open` returning `sqlite3.Connection`), whose methods are then looked up in the catalog. |
| `sync-lock-or-tx` | A `with` over an instance of this class, or over the result of this call, holds a synchronous lock or a database transaction (`EFF106`). |
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
| `copy-sanitizer`, `sync-lock-or-tx`, `non-reentrant-lock`, `one-at-a-time`, `writes-in-order` | not read: library symbols only |

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
