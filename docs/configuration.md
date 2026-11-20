# Configuration

heki reads `[tool.heki]` from `pyproject.toml`, or a `heki.toml` / `.heki.toml` file (same keys
without the `tool.heki` prefix). The nearest file at or above the checked path wins; `--config
<path>` points at one explicitly. Keys are kebab-case, as in Ruff.

```toml
[tool.heki]
target-version = "py312"            # default: inferred from project.requires-python
extend-exclude = ["generated"]      # added to the default excludes
baseline = "heki-baseline.toml"     # Ratchet baseline used by `heki check`

[tool.heki.lint]
select = ["MUT", "EFF", "DATA", "OPT"]
extend-select = ["EFF105"]
ignore = ["OPT110"]
per-file-ignores = { "tests/**" = ["EFF"], "scripts/*.py" = ["OPT"] }
relax-async-in-tests = true

[tool.heki.lint.severity]
OPT103 = "warning"
```

## Top-level settings

| Key | Default | Meaning |
| --- | ------- | ------- |
| `target-version` | the lower bound of `project.requires-python`, else `py312` | Python version of the code: `py311` … `py315` (or `3.11` … `3.15`). Without it, heki reads `requires-python` from the `pyproject.toml` that holds the configuration, or the one next to `heki.toml` / the `--config` file, and takes its oldest admitted version, clamped to 3.11–3.15 (`>=3.9` → `py311`). `--target-version` overrides both. |
| `exclude` | the Ruff defaults (`.git`, `.venv`, `build`, `dist`, `node_modules`, …) | Replaces the excluded paths (globs, matched against paths and path components). |
| `extend-exclude` | `[]` | Adds excluded paths. |
| `baseline` | none | Ratchet baseline file used when `--baseline` is not given. |

`.gitignore` and `.hekiignore` files are honored during discovery. Files named explicitly on the
command line are always checked.

## `[tool.heki.lint]`

| Key | Meaning |
| --- | ------- |
| `select` | Rules to enable instead of the defaults (codes, prefixes such as `EFF`, rule names, or `ALL`). |
| `extend-select` | Rules to add. Opt-in rules (`EFF105`, `EFF106`, `EFF107`, `MUT103`, `MUT106`, `MUT108`) need their exact code or name. |
| `ignore` | Rules to disable (prefixes match every rule they cover). |
| `per-file-ignores` | Glob → rules disabled in matching files (project-relative POSIX paths). |
| `severity` | Rule → `"error"` or `"warning"` (affects output and SARIF level; both make `heki check` fail). |
| `relax-async-in-tests` | `true` (default) skips `EFF101` / `EFF105` in `test_*` functions and files under `tests/`. |

The command line wins: `--select` replaces `select`, `--extend-select` and `--ignore` add to the
configured lists.

## Ownership contracts: `[[tool.heki.write-owners]]` (`MUT201`)

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields", "DataType.data_types"]
allowed-writers = ["pkg.parser.generation.*", "Store.*"]
exempt-methods = ["__deepcopy__", "copy_deep"]
forbid-cached-property-on = ["DataModel"]
message = "use GenerationStore.set_fields() instead"
```

- `targets`: `Class.attr` names. A target matches the fully qualified name or any dotted suffix
  (`DataModel.fields` matches `pkg.model.DataModel.fields`), and globs are allowed. Subclasses of
  a target class are covered too.
- `allowed-writers`: functions allowed to write (fully qualified or qualname, globs allowed).
- `forbidden-writers`: when set, only writes by these functions are reported (matched like
  `allowed-writers`); see [Allowed and forbidden functions](#allowed-and-forbidden-functions).
- `exempt-methods`: construction and copy methods allowed to write the targets. A plain name
  (`__deepcopy__`) matches any method of that name; an entry with a dot or a glob
  (`DataModel.copy_deep`, `create_*`) is matched like `allowed-writers`.
- `forbid-cached-property-on`: classes on which defining a `@cached_property` is itself reported.
- `message`: text appended to every report of the contract (where to write instead).

A class's own initialization is always allowed: an initializer (`__init__`, `__post_init__`,
`__new__`, `model_post_init`, `__setstate__`) of a target class or a subclass may write its own
`self`, directly, through `super().__init__()`, or through helpers it passes `self` to. So is a
function that only runs inside the owner's operations: every path of project callers above it
starts in an allowed writer, an exempt method, or such an initialization.

A write in a private helper (`_name`) with project callers is reported at the call sites of its
first non-private callers; one report stands for every write behind its call, and a Ratchet
baseline counts each of them (`sites` in JSON output).

## Read contracts: `[[tool.heki.read-owners]]` (`MUT203`)

```toml
[[tool.heki.read-owners]]
name = "reference-children"
targets = ["pkg.reference.Reference.children"]
allowed-readers = ["*GenerationIndex*", "*GenerationStore*"]
exempt-methods = ["__deepcopy__"]
message = "read children through GenerationIndex instead"
```

The read side of `write-owners`: reads of a target (`ref.children`, a local alias,
`getattr(ref, "children")`, `ref.__dict__["children"]`) outside `allowed-readers` and
`exempt-methods` are reported. Targets, subclasses, initializers, private helpers, `message`,
and `forbidden-readers` work as for `write-owners`. Reads through a parameter without a
project-class annotation are not seen.

## Call contracts: `[[tool.heki.call-owners]]` (`MUT204`)

```toml
[[tool.heki.call-owners]]
name = "reference-mutators"
targets = ["DataType.replace_reference", "pkg.util.reset_cache", "FinalFacts"]
allowed-callers = ["pkg.parser.generation.*"]
exempt-methods = ["__deepcopy__"]
message = "use a GenerationStore operation instead"
```

The call side of `write-owners`: calls of a target outside `allowed-callers` and
`exempt-methods` are reported.

- `targets`: functions, methods, or classes (dotted names or globs, matched against the fully
  qualified name and any dotted suffix). A `Class.method` target covers the overrides of the
  method in subclasses; a class target means constructing the class or a subclass. A call is
  reported only when every function it may call (each candidate of the class hierarchy) is a
  target, so `text.replace(...)` on a `str` is never a call of `DataType.replace_reference`.
  Bound-method aliases (`replace = dt.replace_reference`) and `getattr(dt, "replace_reference")`
  with a constant name are followed.
- `allowed-callers`, `forbidden-callers`, `exempt-methods`, `message`: as for `write-owners`.

Private helpers, functions that only run inside allowed callers, and a class's own
initialization work as for `write-owners`. The targets of a contract may call each other: a call
of the guarded operation is checked where it is made, so what the operation does inside is part
of it.

## Allowed and forbidden functions

`allowed-writers`, `allowed-readers`, and `allowed-callers` name the functions a contract allows;
without them, every access to a target is reported. `forbidden-writers`, `forbidden-readers`,
and `forbidden-callers` name where the contract applies instead, so a layer rule ("the parser
must not read `Reference.children`") needs no list of every other layer:

```toml
[[tool.heki.read-owners]]
name = "parser-reads"
targets = ["Reference.children"]
forbidden-readers = ["pkg.parser.*"]
allowed-readers = ["pkg.parser.index.*"]
```

The two lists combine as follows:

| `allowed-*` | `forbidden-*` | Reported |
| ----------- | ------------- | -------- |
| unset | unset | every access |
| set | unset | accesses outside the allowed functions |
| unset | set | accesses by the forbidden functions |
| set | set | accesses by the forbidden functions, except the allowed ones |

An access counts as one by a forbidden function when the function that is reported, one of the
private helpers it hands the access through, or the function making the access is forbidden.
`allowed-*` and `exempt-methods` win wherever they apply on that path, as does the rule for
functions that only run inside allowed callers.

## Effect boundaries: `[[tool.heki.effect-boundaries]]` (`EFF201`)

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

| Key | Meaning |
| --- | ------- |
| `scopes` | Modules, classes, or functions (dotted names or globs) whose functions must stay inside the boundary; nested functions are included. |
| `forbid` | `blocking-net`, `blocking-db`, `blocking-fs`, `blocking-subprocess`, `blocking-sync`, `sleep`, `blocking` (all six), `mutates-params` (for a method, `self` is a parameter), `writes-targets`, `reads-targets`, `calls`. An unknown token is a configuration error. |
| `write-targets` | `Class.attr` targets of `writes-targets` (required with it; subclasses included). |
| `read-targets` | `Class.attr` targets of `reads-targets` (required with it; subclasses included): reads as `read-owners` sees them, also inside property getters. |
| `call-targets` | Functions, methods, or classes of `calls` (required with it), matched as the `targets` of `call-owners`. |
| `write-universe` | `Class.attr` entries: a reached write of one that `allow-writes` does not name is reported, once per written `Class.attr`. |
| `allow-writes` | `Class.attr` writes the boundary allows; they are exempt from `write-targets` too. Needs `write-universe`. |
| `allowed-callees` | Functions, classes, modules, or ecosystem symbols whose effects are allowed: paths through them are not followed, and calls of them are no `calls` violation. |
| `message` | Text appended to every report of the boundary. |

Each scope function is checked through any number of calls; a violation inside another scope
function is reported there only, once. One report per scope function and forbidden kind (or
target) stands for every distinct site of it that the function reaches, and a Ratchet baseline
counts each of them.

## Shared return values: `[[tool.heki.readonly-returns]]` (`MUT202`)

```toml
[[tool.heki.readonly-returns]]
name = "schema-graph"
producers = ["pkg.parser.SchemaGraphBuilder.schema"]
deep = true
```

| Key | Meaning |
| --- | ------- |
| `producers` | Functions, methods, external symbols, or attributes (`Class.attr`) whose result every caller shares (dotted names or globs). |
| `deep` | Protect the values below the result too (its attributes, items, and elements). |

Functions decorated with `@lru_cache` / `@cache` are protected without configuration, and so
is every function that returns a protected value, or a part of it, on some path: wrappers and
property getters need no entry of their own.

## Announced changes: `[[tool.heki.invalidation]]` (`MUT109`)

```toml
[[tool.heki.invalidation]]
name = "graph-index"
targets = ["DataType.data_types", "DataModel.fields"]
invalidators = ["GenerationStore.note_changed_models", "GenerationStore.refresh_now"]
exempt-writers = ["pkg.parser.generation.*"]
message = "tell the store which models changed"
```

| Key | Meaning |
| --- | ------- |
| `targets` | `Class.attr` names (fully qualified or a dotted suffix, globs allowed) whose writes must be announced; subclasses of a target class are covered too. |
| `invalidators` | Functions or methods (dotted names or globs) whose call announces the change. A call counts when it runs on every path from the write to the function's normal return, directly, in each branch, behind a boolean flag the path sets after the write, or through a helper that calls one on every path of its own. |
| `exempt-writers` | Functions (dotted names or globs) whose writes need no announcement of their own: the owner's operations, which announce or batch the change themselves, and construction methods. |
| `message` | Text appended to every report of the contract. |

A write is followed through local aliases, container methods, `setattr` / `__dict__`, and
helpers. A private helper passes its unannounced writes to its callers, where one report at
the call stands for every write behind it and a Ratchet baseline counts each of them (`sites`
in JSON output); a function whose every caller announces the change after calling it is not
reported. Writes to objects that did not
exist before the call (the class's own initialization, objects the function constructs or
copies) and to objects it gets from a call need no announcement. Only normal returns count: a
path that ends by raising carries no obligation. At most 64 contracts can be declared.

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
| `async-safe` | Safe to call from `async def`. |
| `offloads-blocking-arg` | Index of the callable argument this function runs on another thread. |
| `excluded` | Never reported (logging and similar). |
| `copy-sanitizer` | Returns a copy (ends `MUT202` protection). |

An override replaces the built-in entry for the same symbol as a whole.

## Inline suppressions

```python
x = load()            # heki: ignore[EFF101]
y = load()            # heki: ignore[EFF101, async-blocking-call]
z = load()            # heki: ignore
w = load()            # noqa: EFF101
```

A suppression applies to the line of the diagnostic. Codes and rule names are accepted; a
comment without codes silences every rule on that line.
