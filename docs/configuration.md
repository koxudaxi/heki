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
  tools, and heki does not read them.
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
- Other layouts need `src`: a subdirectory application whose directories are imported as
  top-level packages (`backend/config/env.py` imported as `config.env`) takes
  `src = ["backend"]`, and a `lib/` layout (`lib/matplotlib`) takes `src = ["lib"]`.

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
| `select` | every rule except the opt-in ones | Rules to enable instead of the defaults. An empty list enables no rule before `extend-select`, as in Ruff. |
| `extend-select` | `[]` | Rules to add. |
| `ignore` | `[]` | Rules to disable. |
| `per-file-ignores` | `{}` | Glob → rules disabled in matching files (project-relative POSIX paths). As in Ruff, `*` also matches `/`: `scripts/*.py` covers `scripts/tools/worker.py` too. |
| `severity` | `"warning"` for the `OPT` rules, `"error"` for the others | Rule → `"error"` or `"warning"`. It changes the output and the SARIF level; both make `heki check` fail. |
| `relax-async-in-tests` | `true` | Skip `EFF101`, `EFF105`, and `EFF106` in `test_*` functions and in files under `tests/`. |

- A rule is named by its code (`EFF101`), its name (`async-blocking-call`), a code prefix
  (`EFF`, `MUT2`), or `ALL`. An unknown rule is a configuration error.
- A prefix or `ALL` enables only the default rules. The opt-in rules (`EFF105`, `EFF106`,
  `EFF107`, `MUT101`, `MUT103`, `MUT106`, `MUT108`, `MUT111`, `MUT112`, `MUT113`) need their
  exact code or name. In `ignore`, a prefix covers every rule it matches.
- The command line wins: `--select` replaces `select`, and `--extend-select` and `--ignore` add
  to the configured lists.

## Contracts

Contracts declare what no type can say. Each kind is an array of tables with its own rule:

| Contract | Declares | Rule |
| -------- | -------- | ---- |
| [`write-owners`](#write-owners) | who may write an attribute | `MUT201`; `MUT205` with `unresolved = "report"` |
| [`read-owners`](#read-owners) | who may read an attribute | `MUT203`; `MUT206` with `unresolved = "report"` |
| [`call-owners`](#call-owners) | who may call a function or construct a class | `MUT204`; `MUT207` with `unresolved = "report"` |
| [`effect-boundaries`](#effect-boundaries) | what a scope must not reach: I/O, writes, reads, or calls | `EFF201` |
| [`readonly-returns`](#readonly-returns) | results every caller shares | `MUT202` |
| [`invalidation`](#invalidation) | writes that must be announced | `MUT109` |

- These rules report nothing until a contract exists, except `MUT202`, which also protects the
  results of `@lru_cache` / `@cache` functions.
- Every name in a contract is a dotted pattern. It matches the full name or any dotted suffix
  (`DataModel.fields` matches `pkg.model.DataModel.fields`, not `pkg.BigModel.fields`), and it
  may be a glob (`*`, `?`, `[...]`, `{a,b}`).
- A `Class.attr` target covers the subclasses of the class too.
- A contract heki cannot use is a configuration error: a target that is not `Class.attr`, an
  invalid glob, or an unknown value of `forbid`, `fresh-objects`, or `unresolved`.
- With `unresolved = "report"`, a contract also reports the accesses it cannot tie to a class,
  each under its own rule (`MUT205` writes, `MUT206` reads, `MUT207` calls): an attribute or
  method named like a target, accessed on an object of unknown class (an unannotated parameter,
  a loop variable, an untyped call result, `Any`, a protocol whose implementations heki cannot
  list). A baseline counts them apart from the violations the typed rules prove.
- An attribute named through a string (`setattr`, `getattr`, `__dict__`) is followed when the
  string is a constant: see [Dynamic names](scope.md#dynamic-names).

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

A container entry (`Class.attr["key"]`):

- matches the writes of that constant key (a literal or a string constant), not a write of the
  whole container or with a computed key;
- is reported once, under the entry, when its container is a target too.

A write into the entries of a target's container (`model.meta["kw"][name] = v`) is a write of
the target.

With `fresh-objects = "allow"`, a write is no violation when it constructs the object it writes:

- the object was allocated in the same call chain: by calling its class, by a copy, or by a
  project function returning a new object;
- nothing can hold it yet: no store, `yield`, closure, or call that may keep it ran before the
  write.

A shallow copy's containers are the original's: only rebinding its attributes constructs it.

With `unresolved = "report"`, `MUT205` reports:

- an attribute named like a target, written on an object of unknown class;
- a computed attribute name (`setattr(obj, name, v)`, `obj.__dict__[name] = v`) written on a
  target class.

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
`getattr(ref, "children")`, or `ref.__dict__["children"]`.

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The contract's name in reports. |
| `targets` | `[]` | `Class.attr` attributes. A container entry (`Class.attr["key"]`) is a write target only, and a configuration error here. |
| `allowed-readers` | `[]` | Functions allowed to read the targets. |
| `forbidden-readers` | `[]` | When set, only reads by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to read the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the reads of a target's attribute name on an object of unknown class are reported under [`MUT206`](rules/unresolved-guarded-read.md). |

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
| `targets` | `[]` | Functions, methods, or classes. A `Class.method` target covers the method's overrides in subclasses; a class target means constructing the class or a subclass. |
| `allowed-callers` | `[]` | Functions allowed to call the targets. |
| `forbidden-callers` | `[]` | When set, only calls by these functions are reported. |
| `exempt-methods` | `[]` | Methods allowed to call the targets, matched as in `write-owners`. |
| `message` | none | The help of every report of the contract. |
| `unresolved` | `"ignore"` | `"ignore"` or `"report"`: whether the calls of a method named like a target (its last name) on an object of unknown class are reported under [`MUT207`](rules/unresolved-guarded-call.md). |

- A call is reported only when every function it may call (each candidate in the class
  hierarchy) is a target: `text.replace(...)` on a `str` is never a call of
  `DataType.replace_reference`.
- Bound-method aliases (`replace = dt.replace_reference`) and `getattr(dt, "replace_reference")`
  with a constant name are followed.
- The targets of a contract may call each other: a call of the guarded operation is checked where
  it is made, so what the operation does inside is part of it.

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
  helper it hands the access through, or the function making the access is forbidden.
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
```

| Key | Default | Meaning |
| --- | ------- | ------- |
| `name` | `""` | The boundary's name in reports. |
| `scopes` | `[]` | Modules, classes, or functions whose functions must stay inside the boundary; nested functions are included. |
| `forbid` | `[]` | What the scope functions must not reach (tokens below). |
| `write-targets` | `[]` | `Class.attr` targets of `writes-targets`, required with it. `Class.attr["key"]` names one entry, as in `write-owners`. |
| `read-targets` | `[]` | `Class.attr` targets of `reads-targets`, required with it: reads as `read-owners` sees them, also inside property getters. A container entry is a configuration error. |
| `call-targets` | `[]` | Functions, methods, or classes of `calls`, required with it, matched as the `targets` of `call-owners`. |
| `write-universe` | `[]` | `Class.attr` entries: a reached write of one that `allow-writes` does not name is reported, once per written `Class.attr`. |
| `allow-writes` | `[]` | `Class.attr` writes the boundary allows, exempt from `write-targets` too. Needs `write-universe`. |
| `allowed-callees` | `[]` | Functions, classes, modules, or ecosystem symbols whose effects are allowed: paths through them are not followed, and calls of them are no `calls` violation. |
| `fresh-objects` | `"allow"` | `"allow"` or `"report"`: whether a write that constructs the object it writes (as in `write-owners`) counts as a write of the boundary. |
| `message` | none | The help of every report of the boundary. |

| `forbid` token | Forbids |
| -------------- | ------- |
| `blocking-net`, `blocking-db`, `blocking-fs`, `blocking-subprocess`, `blocking-sync`, `sleep` | that blocking effect |
| `blocking` | all six blocking effects |
| `mutates-params` | changing a parameter in place (for a method, `self` is a parameter) |
| `writes-targets` | writing a `write-targets` attribute |
| `reads-targets` | reading a `read-targets` attribute |
| `calls` | calling a `call-targets` function or class |

- Each scope function is checked through any number of calls.
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

A write is followed through local aliases, container methods, `setattr` / `__dict__`, and
helpers. A helper changing an argument it does not type writes the field or instance its caller
binds to it. At most 64 `invalidation` contracts can be declared.

### How contracts report

These hold for `write-owners`, `read-owners`, and `call-owners`, and the reporting at callers
holds for `invalidation` too:

- **A class's own initialization is allowed.** An initializer (`__init__`, `__post_init__`,
  `__new__`, `model_post_init`, `__setstate__`) of a target class or a subclass may access its own
  `self`: directly, through `super().__init__()`, or through helpers it passes `self` to.
- **A function that only runs inside allowed functions is allowed.** Every path of project
  callers above it starts in an allowed function, an exempt method, or such an initialization.
- **A private helper reports at its callers.** An access in a private helper (`_name`) with
  project callers is reported at the call sites of its first non-private callers. A private
  function called only by its own recursion reports its accesses itself.
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
| `copy-sanitizer`, `sync-lock-or-tx` | not read: library symbols only |

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
